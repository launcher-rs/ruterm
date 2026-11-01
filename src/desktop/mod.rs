//! 远程桌面：VNC 先落地，RDP / SPICE 后续接入同一个标签骨架。
//!
//! 约定（与终端一致）：网络 IO 全在 tokio 侧，UI 线程只收发消息与贴图。
//! 帧以「整帧 BGRA 快照」交给界面：界面自己按刷新节奏取，取走了才再产一帧，
//! 界面卡住时最多留一帧（旧帧丢掉不排队），内存上限因此是两帧而不是无界队列。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub mod keysym;
pub mod vnc;

/// 建连（DNS + TCP）的等待上限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 发布帧的最小间隔：服务器回帧可以很快，界面不必跟着 60fps 重画。
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// 统计窗口：每秒汇报一次实际帧率。
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// 一次连接参数。密码只在握手时用，连接建立后不保留。
#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    /// 是否与其它客户端共享桌面（VNC 的 shared 标志）。
    pub shared: bool,
}

impl Config {
    /// 状态栏 / 标签标题用的 `host:port`。
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 界面发给会话的指令。
#[derive(Debug)]
pub enum Command {
    /// 指针位置与按钮位图（bit0 左键、bit1 中键、bit2 右键）。
    Pointer { x: u16, y: u16, buttons: u8 },
    /// 按键（X11 keysym）。
    Key { keysym: u32, down: bool },
    /// 本地剪贴板 → 远端。
    Clipboard(String),
    /// 请求一次增量更新（正常由会话自己排，界面基本不用发）。
    Refresh,
    /// 请求整帧（切回标签、自适应时用）。
    RequestFull,
    /// 主动断开。
    Close,
}

/// 会话发回界面的事件。
#[derive(Debug)]
pub enum Event {
    /// 握手完成：桌面尺寸与服务器名确定。
    Connected { width: u16, height: u16, name: String },
    /// `latest` 里刚放了一帧新画面。
    FrameReady,
    /// 每秒一次的实际帧率（toolbar 的 `Nfps`）。
    Fps { fps: f32 },
    /// 远端剪贴板。
    Clipboard(String),
    /// 会话结束（正常关闭或出错），原因就地展示。
    Closed { reason: String },
}

/// 一帧整屏 BGRA 快照。
pub struct Snapshot {
    pub width: u16,
    pub height: u16,
    pub bgra: Vec<u8>,
}

/// 一次远程桌面会话的界面侧句柄。
pub struct Session {
    pub command: UnboundedSender<Command>,
    /// 最新一帧；界面画完 `take` 走，会话见空才产下一帧。
    pub latest: Arc<Mutex<Option<Snapshot>>>,
    pub events: UnboundedReceiver<Event>,
}

/// 连上服务器并起收发两个任务。
///
/// 必须在 tokio 运行时里调用（界面侧走 `background_spawn`）。
pub async fn connect(config: Config) -> Result<Session> {
    let mut stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((config.host.as_str(), config.port)),
    )
    .await
    .context("连接超时")?
    .with_context(|| format!("无法连接到 {}", config.addr()))?;
    let _ = stream.set_nodelay(true);

    let init = tokio::time::timeout(
        vnc::HANDSHAKE_TIMEOUT,
        vnc::handshake(&mut stream, config.password.as_deref(), config.shared),
    )
    .await
    .context("握手超时")??;

    let (mut reader, mut writer) = stream.into_split();
    let (command_tx, command_rx) = unbounded_channel::<Command>();
    let (event_tx, event_rx) = unbounded_channel::<Event>();
    let latest = Arc::new(Mutex::new(None));

    writer
        .write_all(&vnc::set_encodings_message(&vnc::ENCODINGS))
        .await
        .context("发送编码列表失败")?;
    writer
        .write_all(&vnc::update_request_message(
            false,
            init.width,
            init.height,
        ))
        .await
        .context("请求首帧失败")?;

    let size = Arc::new(Mutex::new((init.width, init.height)));
    let _ = event_tx.send(Event::Connected {
        width: init.width,
        height: init.height,
        name: init.name.clone(),
    });

    // 写侧：只做指令 → 字节的翻译，网络慢也只是这里排队。
    let writer_size = size.clone();
    tokio::spawn(async move {
        let mut command_rx = command_rx;
        while let Some(command) = command_rx.recv().await {
            let (width, height) = *writer_size.lock().unwrap();
            let bytes = match command {
                Command::Pointer { x, y, buttons } => vnc::pointer_message(x, y, buttons),
                Command::Key { keysym, down } => vnc::key_message(keysym, down),
                Command::Clipboard(text) => vnc::clipboard_message(&text),
                Command::Refresh => vnc::update_request_message(true, width, height),
                Command::RequestFull => vnc::update_request_message(false, width, height),
                Command::Close => {
                    let _ = writer.shutdown().await;
                    break;
                }
            };
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    // 读侧：解码进帧缓冲，按节流产快照。
    let reader_latest = latest.clone();
    let reader_command = command_tx.clone();
    tokio::spawn(async move {
        let latest = reader_latest;
        let command_tx = reader_command;
        let mut state = match vnc::ClientState::new(init) {
            Ok(state) => state,
            Err(error) => {
                let _ = event_tx.send(Event::Closed {
                    reason: error.to_string(),
                });
                return;
            }
        };
        let mut frames = 0u32;
        let mut stats_at = Instant::now();
        let mut last_frame = Instant::now();
        let mut closed = "连接已关闭".to_string();
        loop {
            let message =
                match tokio::time::timeout(vnc::READ_TIMEOUT, vnc::read_message(&mut reader, &mut state))
                    .await
                {
                    Err(_) => {
                        closed = "服务器长时间没有回帧".to_string();
                        break;
                    }
                    Ok(Err(error)) => {
                        closed = error.to_string();
                        break;
                    }
                    Ok(Ok(message)) => message,
                };
            match message {
                vnc::Message::Update { resized } => {
                    frames += 1;
                    if resized {
                        *size.lock().unwrap() = (state.fb.width, state.fb.height);
                    }
                    // 界面还没取走上一帧就跳过这次拷贝：画面只会追新，不排队。
                    let publish = {
                        let mut slot = latest.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(Snapshot {
                                width: state.fb.width,
                                height: state.fb.height,
                                bgra: state.fb.bgra.clone(),
                            });
                            true
                        } else {
                            false
                        }
                    };
                    if publish {
                        let _ = event_tx.send(Event::FrameReady);
                        last_frame = Instant::now();
                    }
                    if stats_at.elapsed() >= STATS_INTERVAL {
                        let fps = frames as f32 / stats_at.elapsed().as_secs_f32();
                        let _ = event_tx.send(Event::Fps { fps });
                        frames = 0;
                        stats_at = Instant::now();
                    }
                    // 间隔不够就等一会儿再要下一帧：服务器才不会把 CPU 烧在回帧上。
                    let elapsed = last_frame.elapsed();
                    if elapsed < FRAME_INTERVAL {
                        tokio::time::sleep(FRAME_INTERVAL - elapsed).await;
                    }
                    let next = if resized {
                        Command::RequestFull
                    } else {
                        Command::Refresh
                    };
                    if command_tx.send(next).is_err() {
                        break;
                    }
                }
                vnc::Message::Clipboard(text) => {
                    let _ = event_tx.send(Event::Clipboard(text));
                }
                vnc::Message::Ignored => {}
            }
        }
        let _ = event_tx.send(Event::Closed { reason: closed });
    });

    Ok(Session {
        command: command_tx,
        latest,
        events: event_rx,
    })
}
