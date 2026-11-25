//! 远程桌面：VNC 与 RDP 共用同一套标签骨架与帧管道。
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
pub mod rdp;
pub mod scancode;
pub mod vnc;

/// 建连（DNS + TCP）的等待上限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 发布帧的最小间隔：服务器回帧可以很快，界面不必跟着 60fps 重画。
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// 统计窗口：每秒汇报一次实际帧率。
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// 远程桌面协议。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Vnc,
    Rdp,
}

impl Protocol {
    /// 表单切换协议时预填的默认端口。
    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Vnc => 5900,
            Protocol::Rdp => 3389,
        }
    }
}

/// 一次连接参数。密码只在握手时用，连接建立后不保留。
#[derive(Clone, Debug)]
pub struct Config {
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    /// 是否与其它客户端共享桌面（VNC 的 shared 标志）。
    pub shared: bool,
    /// RDP 账号；VNC 不用。
    pub username: Option<String>,
    /// RDP 域（可留空）；VNC 不用。
    pub domain: Option<String>,
    /// RDP 请求的桌面尺寸（远端会按这个分辨率建会话）。
    pub size: (u16, u16),
}

impl Config {
    /// 状态栏 / 标签标题用的 `host:port`。
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 一次按键在两种协议下的编码。
///
/// VNC 只认 X11 keysym；RDP 优先扫描码，查不到时用 Unicode 字符。
/// 两边都取不到（纯修饰组合之类）时 `keysym` 为 0，由协议层自己跳过。
#[derive(Clone, Copy, Debug)]
pub struct KeyInput {
    pub keysym: u32,
    pub scancode: Option<u16>,
    pub ch: Option<char>,
}

impl KeyInput {
    /// rgpui 的按键事件 → 协议无关编码。
    pub fn from_event(key: &str, key_char: Option<&str>) -> Option<Self> {
        let keysym = keysym::event_keysym(key, key_char);
        let scancode = scancode::event_scancode(key, key_char);
        let ch = if scancode.is_none() {
            scancode::event_char(key, key_char)
        } else {
            None
        };
        if keysym.is_none() && scancode.is_none() && ch.is_none() {
            return None;
        }
        Some(Self {
            keysym: keysym.unwrap_or(0),
            scancode,
            ch,
        })
    }

    /// 修饰键 / CapsLock 的按下抬起：keysym 给 VNC，扫描码给 RDP。
    pub fn modifier(name: &str) -> Self {
        Self {
            keysym: keysym::named_keysym(name).unwrap_or(0),
            scancode: scancode::named_scancode(name),
            ch: None,
        }
    }
}

/// 界面发给会话的指令。
#[derive(Debug)]
pub enum Command {
    /// 指针位置与按钮位图（bit0 左键、bit1 中键、bit2 右键）。
    Pointer { x: u16, y: u16, buttons: u8 },
    /// 按键。`down` 区分按下与抬起。
    Key { key: KeyInput, down: bool },
    /// 滚轮（远端像素坐标；向上为 `up`）。VNC 由协议层翻成 4/5 号按钮。
    Wheel {
        x: u16,
        y: u16,
        up: bool,
        buttons: u8,
    },
    /// 本地剪贴板 → 远端。
    Clipboard(String),
    /// 把本地文件放上系统剪贴板并让远端粘贴（RDP 文件剪贴板；VNC 忽略）。
    PasteFiles(Vec<std::path::PathBuf>),
    /// 请求一次增量更新（正常由会话自己排，界面基本不用发）。
    Refresh,
    /// 请求整帧（切回标签、自适应时用）。
    RequestFull,
    /// 让远端把桌面改成指定像素尺寸（RDP 动态分辨率；VNC 忽略）。
    Resize {
        width: u16,
        height: u16,
        scale_factor: u32,
    },
    /// 释放该会话已发送的所有键和鼠标按钮。
    ReleaseAll,
    /// 主动断开。
    Close,
}

/// 会话发回界面的事件。
#[derive(Debug)]
pub enum Event {
    /// 握手完成：桌面尺寸与服务器名确定。
    Connected {
        width: u16,
        height: u16,
        name: String,
    },
    /// `latest` 里刚放了一帧新画面。
    FrameReady,
    /// 每秒一次的实际帧率（toolbar 的 `Nfps`）。
    Fps { fps: f32 },
    /// 拖放文件上传进度（RDP 文件剪贴板）：已发送 / 总字节、文件数、是否结束。
    Upload {
        done: u64,
        total: u64,
        files: u32,
        finished: bool,
        failed: bool,
    },
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
    /// RDP 的系统剪贴板授权令牌；界面只允许当前活动桌面会话持有它。
    pub clipboard_token: Option<u64>,
}

/// 按协议连上服务器，交出统一的事件与指令通道。
///
/// 必须在 tokio 运行时里调用（界面侧走 `background_spawn`）。
pub async fn connect(config: Config) -> Result<Session> {
    match config.protocol {
        Protocol::Vnc => connect_vnc(config).await,
        Protocol::Rdp => rdp::connect(config).await,
    }
}

/// VNC（RFB 3.8）：握手 → 起收发两个任务。
async fn connect_vnc(config: Config) -> Result<Session> {
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

    start_vnc(stream, init).await
}

async fn start_vnc<S>(stream: S, init: vnc::ServerInit) -> Result<Session>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (command_tx, mut command_rx) = unbounded_channel::<Command>();
    let (event_tx, event_rx) = unbounded_channel::<Event>();
    let latest = Arc::new(Mutex::new(None));

    writer
        .write_all(&vnc::set_encodings_message(&vnc::ENCODINGS))
        .await
        .context("发送编码列表失败")?;
    writer
        .write_all(&vnc::update_request_message(false, init.width, init.height))
        .await
        .context("请求首帧失败")?;

    let size = Arc::new(Mutex::new((init.width, init.height)));
    let _ = event_tx.send(Event::Connected {
        width: init.width,
        height: init.height,
        name: init.name.clone(),
    });

    // 两个后台任务只通过内部通道与监督任务说话：外部 Session 句柄一旦放下，
    // 监督任务立刻取消读、写两侧，内部续帧请求不会把已经关掉的会话吊住。
    let (writer_tx, mut writer_rx) = unbounded_channel::<Command>();
    let (end_tx, mut end_rx) = unbounded_channel::<String>();

    let writer_size = size.clone();
    let writer_end = end_tx.clone();
    let writer_task = tokio::spawn(async move {
        let mut input = VncInput::default();
        while let Some(command) = writer_rx.recv().await {
            let (width, height) = *writer_size
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let bytes = match command {
                Command::Pointer { x, y, buttons } => {
                    input.pointer(x, y, buttons);
                    vnc::pointer_message(x, y, buttons)
                }
                Command::Key { key, down } => {
                    if key.keysym == 0 {
                        continue;
                    }
                    input.key(key.keysym, down);
                    vnc::key_message(key.keysym, down)
                }
                // VNC 没有独立的滚轮事件：滚轮就是按钮 4/5 的按下与释放。
                // 按钮位图里 4 号按钮是 bit3（0b1000），5 号是 bit4（0b10000）。
                Command::Wheel { x, y, up, buttons } => {
                    let wheel = if up { 1 << 3 } else { 1 << 4 };
                    input.pointer(x, y, buttons);
                    let mut bytes = vnc::pointer_message(x, y, buttons | wheel);
                    bytes.extend_from_slice(&vnc::pointer_message(x, y, buttons));
                    bytes
                }
                Command::Clipboard(text) => vnc::clipboard_message(&text),
                Command::Refresh => vnc::update_request_message(true, width, height),
                Command::RequestFull => vnc::update_request_message(false, width, height),
                // 松开所有仍按着的键和鼠标按钮；失焦 / 退出键鼠捕获时由界面下发。
                Command::ReleaseAll => input.release_all(),
                // VNC 的动态分辨率要协商 ExtendedDesktopSize，这里不做；忽略即可。
                Command::Resize { .. } => continue,
                // RFB 没有文件剪贴板，界面侧已提前拦下并提示。
                Command::PasteFiles(_) => continue,
                Command::Close => break,
            };
            if tokio::time::timeout(vnc::WRITE_TIMEOUT, writer.write_all(&bytes))
                .await
                .map_or(true, |result| result.is_err())
            {
                let _ = writer_end.send("向远端写入失败或超时".to_string());
                return;
            }
        }
    });

    // 读侧：解码进帧缓冲，按节流产快照。
    let reader_latest = latest.clone();
    let reader_size = size.clone();
    let reader_writer = writer_tx.clone();
    let reader_event = event_tx.clone();
    let reader_end = end_tx.clone();
    let reader_task = tokio::spawn(async move {
        let mut state = match vnc::ClientState::new(init) {
            Ok(state) => state,
            Err(error) => {
                let _ = reader_end.send(error.to_string());
                return;
            }
        };
        let mut frames = 0u32;
        let mut stats_at = Instant::now();
        let reason = loop {
            let message = match tokio::time::timeout(
                vnc::READ_TIMEOUT,
                vnc::read_message(&mut reader, &mut state),
            )
            .await
            {
                Err(_) => break "服务器长时间没有响应".to_string(),
                Ok(Err(error)) => break error.to_string(),
                Ok(Ok(message)) => message,
            };
            match message {
                vnc::Message::Update { resized } => {
                    if resized {
                        // 槽位数据本身没有可破坏的不变量：中毒就照用，
                        // 别让协议任务因 UI 侧一次 panic 而静默挂死。
                        *reader_size
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            (state.fb.width, state.fb.height);
                    }
                    // 节流锚点按「收到更新」计：上一帧还没被取走（界面忙）时
                    // 也照常走下面的睡眠，否则会立即再要下一帧，解码线程空烧
                    // CPU、弱网输入流被移动事件排队在后。
                    let received_at = Instant::now();
                    // 界面还没取走上一帧就跳过这次拷贝：画面只会追新，不排队。
                    // 8MB 整帧拷贝放锁外完成：界面取快照只拿一下锁，不被搬运
                    // 挡住（与 rdp.rs 的 publish_frame 同一模式）。
                    let publish = {
                        let slot_free = reader_latest.lock().map_or_else(
                            |poisoned| poisoned.into_inner().is_none(),
                            |slot| slot.is_none(),
                        );
                        if slot_free {
                            let bgra = state.fb.bgra.clone();
                            let mut slot = reader_latest
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            // 槽位只有本任务写；拷贝期间界面只会把它取空，照旧确认。
                            if slot.is_none() {
                                *slot = Some(Snapshot {
                                    width: state.fb.width,
                                    height: state.fb.height,
                                    bgra,
                                });
                            }
                            true
                        } else {
                            false
                        }
                    };
                    // fps 只统计真正发布出去的帧：界面没取走的更新不算数，
                    // 状态栏的数字才是「实际显示帧率」。
                    if publish {
                        frames += 1;
                        let _ = reader_event.send(Event::FrameReady);
                    }
                    if stats_at.elapsed() >= STATS_INTERVAL {
                        let fps = frames as f32 / stats_at.elapsed().as_secs_f32();
                        let _ = reader_event.send(Event::Fps { fps });
                        frames = 0;
                        stats_at = Instant::now();
                    }
                    // 间隔不够就等一会儿再要下一帧：服务器才不会把 CPU 烧在回帧上。
                    let elapsed = received_at.elapsed();
                    if elapsed < FRAME_INTERVAL {
                        tokio::time::sleep(FRAME_INTERVAL - elapsed).await;
                    }
                    let next = if resized {
                        Command::RequestFull
                    } else {
                        Command::Refresh
                    };
                    if reader_writer.send(next).is_err() {
                        break "连接已关闭".to_string();
                    }
                }
                vnc::Message::Clipboard(text) => {
                    let _ = reader_event.send(Event::Clipboard(text));
                }
                vnc::Message::Ignored => {}
            }
        };
        let _ = reader_end.send(reason);
    });

    tokio::spawn(async move {
        let reason = loop {
            tokio::select! {
                command = command_rx.recv() => match command {
                    None | Some(Command::Close) => break "连接已关闭".to_string(),
                    Some(command) => {
                        if writer_tx.send(command).is_err() {
                            break "连接已关闭".to_string();
                        }
                    }
                },
                Some(reason) = end_rx.recv() => break reason,
            }
        };
        writer_task.abort();
        reader_task.abort();
        let _ = event_tx.send(Event::Closed { reason });
    });

    Ok(Session {
        command: command_tx,
        latest,
        events: event_rx,
        clipboard_token: None,
    })
}

/// VNC 侧需要记住「按着什么」，才能在失焦 / 关闭捕获时把远端按键全部抬起。
#[derive(Default)]
struct VncInput {
    keys: std::collections::BTreeSet<u32>,
    x: u16,
    y: u16,
    buttons: u8,
}

impl VncInput {
    fn pointer(&mut self, x: u16, y: u16, buttons: u8) {
        self.x = x;
        self.y = y;
        self.buttons = buttons;
    }

    fn key(&mut self, keysym: u32, down: bool) {
        if down {
            self.keys.insert(keysym);
        } else {
            self.keys.remove(&keysym);
        }
    }

    fn release_all(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for keysym in std::mem::take(&mut self.keys) {
            bytes.extend_from_slice(&vnc::key_message(keysym, false));
        }
        if self.buttons != 0 {
            self.buttons = 0;
            bytes.extend_from_slice(&vnc::pointer_message(self.x, self.y, 0));
        }
        bytes
    }
}
