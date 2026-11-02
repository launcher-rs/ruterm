//! RDP 会话（IronRDP 客户端）。
//!
//! `RdpClient::run` 的 future 不是 `Send`，整条会话跑在独立线程的 current-thread
//! 运行时上；与界面之间只用 tokio 通道和一个帧槽位对接，界面线程不做任何网络 IO。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use ironrdp::client::config::{ConfigBuilder, Destination};
use ironrdp::client::rdp::{RdpClient, RdpInputEvent, RdpOutputEvent};
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
#[cfg(windows)]
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
};

use super::{Command, Config, Event, KeyInput, Session, Snapshot};

/// 发出 Close 后等服务器优雅收尾的上限；超过就直接丢连接。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// 每秒汇报一次实际帧率。
const FPS_INTERVAL: Duration = Duration::from_secs(1);
/// 快照发布的最小间隔：RDP 的增量更新很密，整帧转换与纹理上传不必跟着每个包走。
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// 剪贴板监听窗口的 Win32 消息泵间隔（见 `pump_messages`）。
const PUMP_INTERVAL: Duration = Duration::from_millis(10);

/// 连上服务器：建好客户端配置后把会话交给专用线程。
pub async fn connect(config: Config) -> Result<Session> {
    let client_config = build_client_config(&config)?;
    let name = config.host.clone();

    let (command_tx, command_rx) = unbounded_channel::<Command>();
    let (event_tx, event_rx) = unbounded_channel::<Event>();
    let latest = std::sync::Arc::new(Mutex::new(None));

    let thread_latest = latest.clone();
    std::thread::Builder::new()
        .name("ruterm-rdp".to_string())
        .spawn(move || {
            if let Err(error) = run_session(client_config, name, command_rx, &event_tx, &thread_latest) {
                let _ = event_tx.send(Event::Closed {
                    reason: format!("{error:#}"),
                });
            }
        })
        .context("创建 RDP 会话线程失败")?;

    Ok(Session {
        command: command_tx,
        latest,
        events: event_rx,
    })
}

fn build_client_config(config: &Config) -> Result<ironrdp::client::config::Config> {
    let mut builder = ConfigBuilder::new()
        .with_destination(Destination::from_parts(config.host.clone(), config.port))
        .with_username(config.username.clone().unwrap_or_default())
        .with_password(config.password.clone().unwrap_or_default())
        .with_client_name("RuTerm")
        .with_client_dir("")
        .with_client_build(0)
        .with_platform(MajorPlatformType::WINDOWS)
        .with_desktop_width(config.size.0.max(200))
        .with_desktop_height(config.size.1.max(200))
        .with_color_depth(32)
        .with_credssp(true)
        .with_tls(true)
        .with_autologon(true)
        .with_compression(true)
        // Windows 后端直接挂系统剪贴板：文本与文件（CF_HDROP / 文件流）双向同步。
        .with_clipboard(ironrdp::client::config::ClipboardType::Enable);
    if let Some(domain) = config.domain.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
        builder = builder.with_domain(domain);
    }
    builder.build().context("RDP 参数无效")
}

/// 线程主体：在 current-thread 运行时里跑客户端，同时搬运指令与帧。
fn run_session(
    client_config: ironrdp::client::config::Config,
    name: String,
    mut command_rx: UnboundedReceiver<Command>,
    event_tx: &UnboundedSender<Event>,
    latest: &Mutex<Option<Snapshot>>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("创建 RDP 运行时失败")?;
    // IronRDP 的剪贴板监听窗口在本线程创建，Win32 消息也只能在本线程泵；
    // 先把线程挂到 OLE STA，远端复制的文件才能作为数据对象放进本机剪贴板。
    #[cfg(windows)]
    let _com = ComApartment::new();
    runtime.block_on(async {
        let (output_tx, mut output_rx) = tokio::sync::mpsc::channel::<RdpOutputEvent>(1);
        let client = RdpClient::new(client_config, output_tx);
        let input = client.input_sender();
        let session = client.run();
        tokio::pin!(session);

        let mut database = Database::new();
        let mut frame: Option<Frame> = None;
        let mut announced = false;
        let mut frames = 0u32;
        let mut stats_at = Instant::now();
        let mut close_at: Option<Instant> = None;
        // 上一帧发布时间与下一次允许发布的截止点：密集更新期间只保留最新整帧，
        // 界面还没取走上一帧时也自然跳过（槽位非空）。
        let mut last_publish = Instant::now()
            .checked_sub(FRAME_INTERVAL)
            .unwrap_or_else(Instant::now);
        let mut pending_at: Option<Instant> = None;
        // 持续存在的计时器：select! 每轮都会重建其它 future，用 sleep 会在密集帧
        // 期间被反复重置而饿死消息泵。
        let mut pump = tokio::time::interval(PUMP_INTERVAL);
        pump.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = &mut session => {
                    // 收尾前排空队列：Terminated 往往就在里面等着。
                    while let Ok(event) = output_rx.try_recv() {
                        if matches!(
                            handle_output(event, event_tx, &mut frame, &mut announced, &name, &mut frames),
                            Output::Ended
                        ) {
                            return;
                        }
                    }
                    // 最后一帧可能还在等节流窗口：结束前补发，别把画面停在上一帧。
                    flush_frame(event_tx, latest, &frame, &mut last_publish);
                    let _ = event_tx.send(Event::Closed {
                        reason: "RDP 会话已结束".to_string(),
                    });
                    return;
                }
                event = output_rx.recv() => {
                    let Some(event) = event else { return };
                    match handle_output(event, event_tx, &mut frame, &mut announced, &name, &mut frames) {
                        Output::Ended => return,
                        Output::Dirty => {
                            if last_publish.elapsed() >= FRAME_INTERVAL {
                                flush_frame(event_tx, latest, &frame, &mut last_publish);
                                pending_at = None;
                            } else {
                                pending_at.get_or_insert(last_publish + FRAME_INTERVAL);
                            }
                        }
                        Output::Alive => {}
                    }
                    report_fps(event_tx, &mut frames, &mut stats_at);
                }
                command = command_rx.recv() => {
                    let Some(command) = command else {
                        // 界面丢弃了指令通道（标签已关）：直接断开。
                        let _ = input.send(RdpInputEvent::Close);
                        return;
                    };
                    if handle_command(command, &mut database, &input, latest, frame.as_ref()) {
                        close_at = Some(Instant::now() + CLOSE_TIMEOUT);
                    }
                }
                _ = async {
                    if let Some(deadline) = pending_at {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if pending_at.is_some() => {
                    flush_frame(event_tx, latest, &frame, &mut last_publish);
                    pending_at = None;
                }
                _ = async {
                    if let Some(deadline) = close_at {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if close_at.is_some() => {
                    return;
                }
                // 剪贴板监听窗口与 OLE 的跨线程调用都靠本线程泵消息。
                _ = pump.tick() => {
                    pump_messages();
                }
            }
        }
    });
    Ok(())
}

/// 会话线程的 OLE 单线程套间；线程退出时在同一线程卸载。
#[cfg(windows)]
struct ComApartment(bool);

#[cfg(windows)]
impl ComApartment {
    fn new() -> Self {
        // SAFETY: 会话线程新建，尚未初始化 COM；卸载在 Drop 里同线程完成。
        Self(unsafe { OleInitialize(None) }.is_ok())
    }
}

#[cfg(windows)]
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: 与 `OleInitialize` 在同一线程配对。
            unsafe { OleUninitialize() };
        }
    }
}

/// 泵一轮本线程的 Win32 消息（空窗口句柄＝只取本线程的）。
#[cfg(windows)]
fn pump_messages() {
    let mut message = MSG::default();
    // SAFETY: 标准的线程消息泵。
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        // SAFETY: 消息由 `PeekMessageW` 取到，按常规分发。
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// 非 Windows 没有剪贴板监听窗口，无需泵消息。
#[cfg(not(windows))]
fn pump_messages() {}

/// 一条服务器输出对会话的影响。
enum Output {
    /// 无需推帧（指针、心跳等）。
    Alive,
    /// 画面已更新，按节流规则决定何时发布。
    Dirty,
    /// 连接结束。
    Ended,
}

/// 把 `frame` 里最新整帧转成快照发布（槽位已有帧时放弃这次）。
fn flush_frame(
    event_tx: &UnboundedSender<Event>,
    latest: &Mutex<Option<Snapshot>>,
    frame: &Option<Frame>,
    last_publish: &mut Instant,
) {
    *last_publish = Instant::now();
    if let Some(current) = frame.as_ref()
        && publish_frame(latest, current)
    {
        let _ = event_tx.send(Event::FrameReady);
    }
}

type Frame = (Vec<u32>, u16, u16);

/// 处理一条服务器输出；返回 `true` 表示会话已结束。
fn handle_output(
    event: RdpOutputEvent,
    event_tx: &UnboundedSender<Event>,
    frame: &mut Option<Frame>,
    announced: &mut bool,
    name: &str,
    frames: &mut u32,
) -> Output {
    match event {
        RdpOutputEvent::Image {
            buffer,
            width,
            height,
        } => {
            let (width, height) = (width.get(), height.get());
            *frames += 1;
            if !*announced {
                *announced = true;
                let _ = event_tx.send(Event::Connected {
                    width,
                    height,
                    name: name.to_string(),
                });
            }
            *frame = Some((buffer, width, height));
            Output::Dirty
        }
        RdpOutputEvent::ConnectionFailure(error) => {
            let _ = event_tx.send(Event::Closed {
                reason: error.to_string(),
            });
            Output::Ended
        }
        RdpOutputEvent::Terminated(result) => {
            let reason = match result {
                Ok(reason) => reason.to_string(),
                Err(error) => error.to_string(),
            };
            let _ = event_tx.send(Event::Closed { reason });
            Output::Ended
        }
        // 远端指针由服务端绘制在画面里；本机鼠标形状照旧，不用这些事件。
        RdpOutputEvent::PointerDefault
        | RdpOutputEvent::PointerHidden
        | RdpOutputEvent::PointerPosition { .. }
        | RdpOutputEvent::PointerBitmap(_) => Output::Alive,
    }
}

fn report_fps(event_tx: &UnboundedSender<Event>, frames: &mut u32, stats_at: &mut Instant) {
    if stats_at.elapsed() < FPS_INTERVAL {
        return;
    }
    let fps = *frames as f32 / stats_at.elapsed().as_secs_f32();
    let _ = event_tx.send(Event::Fps { fps });
    *frames = 0;
    *stats_at = Instant::now();
}

/// 把整屏 `0x00RRGGBB` 转成界面用的 BGRA；界面还没取走上一帧就丢掉这次转换。
fn publish_frame(latest: &Mutex<Option<Snapshot>>, frame: &Frame) -> bool {
    let Ok(mut slot) = latest.lock() else {
        return false;
    };
    if slot.is_some() {
        return false;
    }
    let (buffer, width, height) = frame;
    // 源像素是 0x00RRGGBB；补上不透明 alpha 后按小端写出的字节序正好是 B,G,R,A，
    // 整块只需一次按 4 字节步进的拷贝，逐通道拆分的开销省掉。
    let mut bgra = vec![0u8; buffer.len() * 4];
    for (pixel, out) in buffer.iter().zip(bgra.as_chunks_mut::<4>().0) {
        out.copy_from_slice(&(*pixel | 0xFF00_0000).to_le_bytes());
    }
    *slot = Some(Snapshot {
        width: *width,
        height: *height,
        bgra,
    });
    true
}

/// 处理一条界面指令；返回 `true` 表示收到断开请求。
fn handle_command(
    command: Command,
    database: &mut Database,
    input: &UnboundedSender<RdpInputEvent>,
    latest: &Mutex<Option<Snapshot>>,
    frame: Option<&Frame>,
) -> bool {
    match command {
        Command::Close => {
            let _ = input.send(RdpInputEvent::Close);
            true
        }
        Command::Pointer { x, y, buttons } => {
            send_input(database, input, pointer_operations(database, x, y, buttons));
            false
        }
        Command::Key { key, down } => {
            if let Some(operation) = key_operation(key, down) {
                send_input(database, input, [operation]);
            }
            false
        }
        Command::Wheel { x, y, up, .. } => {
            let operations = [
                Operation::MouseMove(MousePosition { x, y }),
                Operation::WheelRotations(WheelRotations {
                    is_vertical: true,
                    rotation_units: if up { 1 } else { -1 },
                }),
            ];
            send_input(database, input, operations);
            false
        }
        Command::Resize {
            width,
            height,
            scale_factor,
        } => {
            let _ = input.send(RdpInputEvent::Resize {
                width,
                height,
                scale_factor,
                physical_size: None,
            });
            false
        }
        // 剪贴板由 WinClipboard 后端直接挂系统剪贴板（文本与文件都自动双向同步）；
        // 界面侧再发一遍会覆盖远端刚复制的内容，这里忽略。
        Command::Clipboard(_) => false,
        // RDP 只在画面变化时推帧；界面要整帧时把最后一帧再放回槽位。
        Command::Refresh | Command::RequestFull => {
            if let Some(frame) = frame {
                publish_frame(latest, frame);
            }
            false
        }
    }
}

fn send_input(
    database: &mut Database,
    input: &UnboundedSender<RdpInputEvent>,
    operations: impl IntoIterator<Item = Operation>,
) {
    let events = database.apply(operations);
    if !events.is_empty() {
        let _ = input.send(RdpInputEvent::FastPath(events));
    }
}

fn pointer_operations(database: &Database, x: u16, y: u16, buttons: u8) -> Vec<Operation> {
    let mut operations = Vec::with_capacity(5);
    for (bit, button) in [
        (0b001, MouseButton::Left),
        (0b010, MouseButton::Middle),
        (0b100, MouseButton::Right),
    ] {
        let pressed = buttons & bit != 0;
        if database.is_mouse_button_pressed(button) != pressed {
            operations.push(if pressed {
                Operation::MouseButtonPressed(button)
            } else {
                Operation::MouseButtonReleased(button)
            });
        }
    }
    operations.push(Operation::MouseMove(MousePosition { x, y }));
    operations
}

fn key_operation(key: KeyInput, down: bool) -> Option<Operation> {
    if let Some(scancode) = key.scancode {
        let scancode = Scancode::from_u16(scancode);
        return Some(if down {
            Operation::KeyPressed(scancode)
        } else {
            Operation::KeyReleased(scancode)
        });
    }
    let character = key.ch?;
    Some(if down {
        Operation::UnicodeKeyPressed(character)
    } else {
        Operation::UnicodeKeyReleased(character)
    })
}
