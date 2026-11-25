//! RDP 会话（IronRDP 客户端）。
//!
//! `RdpClient::run` 的 future 不是 `Send`，整条会话跑在独立线程的 current-thread
//! 运行时上；与界面之间只用 tokio 通道和一个帧槽位对接，界面线程不做任何网络 IO。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use ironrdp::client::config::{ConfigBuilder, Destination};
use ironrdp::client::rdp::{RdpClient, RdpInputEvent, RdpOutputEvent};
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_cliprdr_native::upload::{UploadProgress, begin_upload, cancel_upload};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::{Command, Config, Event, KeyInput, Session, Snapshot};

/// 发出 Close 后等服务器优雅收尾的上限；超过就直接丢连接。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// 每秒汇报一次实际帧率。
const FPS_INTERVAL: Duration = Duration::from_secs(1);
/// 快照发布的最小间隔：RDP 的增量更新很密，整帧转换与纹理上传不必跟着每个包走。
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// 文件列表进剪贴板后最多等这么久，再替用户按 Ctrl+V（正常情况远端一确认就按）。
const PASTE_FALLBACK: Duration = Duration::from_secs(2);
/// 上传进度的轮询间隔。
const UPLOAD_TICK: Duration = Duration::from_millis(200);
/// 开始接收后，多久没有新字节就认为传输停了。
const UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// 远端已回执、但一直没开始读字节：多半是卡在同名文件覆盖确认框上，等人点。
const UPLOAD_START_TIMEOUT: Duration = Duration::from_secs(120);
/// 剪贴板线程枚举大目录的宽限；超过还没认出这批文件就当登记失败。
const UPLOAD_ENUM_TIMEOUT: Duration = Duration::from_secs(60);
/// 连接后观察首屏的时长：这段时间内若桌面几乎全黑，说明远端只有图标、没有重画壁纸。
const WALLPAPER_CHECK_DELAY: Duration = Duration::from_millis(900);
/// 判定「只有图标的黑底」的纯黑像素比例阈值。正常壁纸桌面远低于此值。
const WALLPAPER_BLACK_THRESHOLD: f64 = 0.80;

/// 连上服务器：建好客户端配置后把会话交给专用线程。
pub async fn connect(config: Config) -> Result<Session> {
    let client_config = build_client_config(&config)?;
    let name = config.host.clone();

    let (command_tx, command_rx) = unbounded_channel::<Command>();
    let (event_tx, event_rx) = unbounded_channel::<Event>();
    let latest = std::sync::Arc::new(Mutex::new(None));
    let clipboard_token = ironrdp_cliprdr_native::allocate_clipboard_session();

    let thread_latest = latest.clone();
    let spawned = std::thread::Builder::new()
        .name("ruterm-rdp".to_string())
        .spawn(move || {
            // WinClipboard is built on this thread inside `RdpClient::run`.
            ironrdp_cliprdr_native::prepare_clipboard_session(clipboard_token);
            let result = run_session(client_config, name, command_rx, &event_tx, &thread_latest);
            ironrdp_cliprdr_native::release_clipboard_session(clipboard_token);
            if let Err(error) = result {
                let _ = event_tx.send(Event::Closed {
                    reason: format!("{error:#}"),
                });
            }
        });
    if let Err(error) = spawned {
        // 线程没起来，剪贴板会话登记就没人释放了：确定性泄漏路径，补上。
        ironrdp_cliprdr_native::release_clipboard_session(clipboard_token);
        return Err(anyhow::Error::new(error).context("创建 RDP 会话线程失败"));
    }

    Ok(Session {
        command: command_tx,
        latest,
        events: event_rx,
        clipboard_token: Some(clipboard_token),
    })
}

fn build_client_config(config: &Config) -> Result<ironrdp::client::config::Config> {
    let mut builder = ConfigBuilder::new()
        .with_destination(Destination::from_parts(config.host.clone(), config.port))
        .with_username(config.username.clone().unwrap_or_default())
        .with_password(config.password.clone().unwrap_or_default())
        .with_client_name("RuTerm")
        // 与 mstsc / FreeRDP 的握手值保持一致，避免服务端把客户端识别成老版本。
        // 这不是壁纸问题的根因（系统 mstsc 同样会复现），只是消除无谓的兼容分支。
        .with_client_dir("C:\\Windows\\System32\\mstscax.dll")
        .with_client_build(22631)
        .with_platform(MajorPlatformType::WINDOWS)
        .with_desktop_width(config.size.0.max(200))
        .with_desktop_height(config.size.1.max(200))
        .with_color_depth(32)
        // 只广告 RemoteFX。本项目没有编译 IronRDP 的 qoi/qoiz feature，所以默认
        // 也只有 RemoteFX。注意 `client_codecs_capabilities` 对未列出的 codec 用
        // `unwrap_or(true)`，单靠 `remotefx:on` 无法排除将来编译进来的 qoi/qoiz；
        // 真正守住这一点的是单测 `client_config_advertises_only_remotefx`。
        .with_codecs(vec!["remotefx:on".to_owned()])
        .with_credssp(true)
        // Only disable legacy TLS + graphical login; CredSSP still uses validated TLS.
        .with_tls(false)
        // 与 mstsc / FreeRDP 的默认行为一致。CredSSP 下这个标志会被服务端忽略，
        // 但保留 false 可以避免以后改用其它认证方式时意外改变会话行为。
        .with_autologon(false)
        .with_compression(true)
        // Windows 后端直接挂系统剪贴板：文本与文件（CF_HDROP / 文件流）双向同步。
        .with_clipboard(ironrdp::client::config::ClipboardType::Enable);
    if let Some(domain) = config
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
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
    runtime.block_on(async {
        let (output_tx, mut output_rx) = tokio::sync::mpsc::channel::<RdpOutputEvent>(1);
        let client = RdpClient::new(client_config, output_tx);
        let input = client.input_sender();
        let session = client.run();
        tokio::pin!(session);

        let mut database = Database::new();
        let mut held = HeldInput::default();
        let mut frame: Option<Frame> = None;
        let mut announced = false;
        let mut frames = 0u32;
        // 壁纸自愈：首屏若只有图标（几乎全黑），等远端画完首帧后按一次 F5，
        // 强制资源管理器重画桌面。只做一次，避免干扰用户后续操作。
        let mut wallpaper_check_at: Option<Instant> = None;
        let mut stats_at = Instant::now();
        let mut close_at: Option<Instant> = None;
        // 上一帧发布时间与下一次允许发布的截止点：密集更新期间只保留最新整帧，
        // 界面还没取走上一帧时也自然跳过（槽位非空）。
        let mut last_publish = Instant::now()
            .checked_sub(FRAME_INTERVAL)
            .unwrap_or_else(Instant::now);
        let mut pending_at: Option<Instant> = None;
        // 拖放文件：先放进本机剪贴板，等远端确认文件列表后替用户按一次 Ctrl+V；
        // 同时轮询剪贴板线程回报的已发送字节，供界面画进度条。
        let mut upload: Option<UploadState> = None;
        // 下次轮询上传进度的时间（固定节拍，避免被高频帧事件不断重置）。
        let mut upload_tick: Option<Instant> = None;

        loop {
            tokio::select! {
                _ = &mut session => {
                    // 收尾前排空队列：Terminated 往往就在里面等着。
                    while let Ok(event) = output_rx.try_recv() {
                        if matches!(
                            handle_output(
                                event,
                                event_tx,
                                &mut frame,
                                &mut announced,
                                &name,
                                &mut frames,
                                &mut wallpaper_check_at,
                            ),
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
                    let Some(event) = event else {
                        // 客户端任务退出却没走 Terminated（如内部 panic）：输出
                        // 通道直接关闭。不补发 Closed，UI 会永远停在 Live 假在线。
                        let _ = event_tx.send(Event::Closed {
                            reason: "RDP 客户端输出已中断".to_string(),
                        });
                        return;
                    };
                    match handle_output(
                        event,
                        event_tx,
                        &mut frame,
                        &mut announced,
                        &name,
                        &mut frames,
                        &mut wallpaper_check_at,
                    ) {
                        Output::Ended => return,
                        Output::Dirty => {
                            if last_publish.elapsed() >= FRAME_INTERVAL {
                                pending_at = flush_or_retry(event_tx, latest, &frame, &mut last_publish);
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
                        // 界面丢弃了指令通道（标签已关）：直接断开。事件接收端
                        // 多半也已关闭，补发是无害的 no-op，但保险起见路径完整。
                        let _ = input.send(RdpInputEvent::Close);
                        let _ = event_tx.send(Event::Closed {
                            reason: "RDP 会话已随标签关闭".to_string(),
                        });
                        return;
                    };
                    match command {
                        Command::PasteFiles(paths) => {
                            // 连续两次拖放：先取消上一批还挂着的登记，别让两批
                            // 文件流的进度在剪贴板线程里搅在一起。
                            if let Some(previous) = upload.take() {
                                previous.progress.abort();
                            }
                            // 放进本机剪贴板即可：剪贴板线程的监听窗口会把文件列表报给远端。
                            // 先登记进度再写剪贴板，剪贴板线程才认领得到。
                            let progress = begin_upload(&paths);
                            if set_clipboard_files(&paths) {
                                upload = Some(UploadState::new(progress));
                                upload_tick = Some(Instant::now());
                            } else {
                                cancel_upload(&progress);
                                // 本机剪贴板被占用之类：明确告诉界面，别让它停在「已放进剪贴板」。
                                let _ = event_tx.send(Event::Upload {
                                    done: 0,
                                    total: 0,
                                    files: 0,
                                    finished: true,
                                    failed: true,
                                });
                            }
                        }
                        Command::CancelUpload => {
                            if let Some(state) = upload.take() {
                                state.progress.abort();
                                cancel_upload(&state.progress);
                                let _ = event_tx.send(Event::Upload {
                                    done: state.progress.sent(),
                                    total: state.progress.total(),
                                    files: state.progress.files(),
                                    finished: true,
                                    failed: true,
                                });
                            }
                            upload_tick = None;
                        }
                        command => {
                            if handle_command(
                                command,
                                &mut database,
                                &input,
                                latest,
                                frame.as_ref(),
                                &mut held,
                            ) {
                                close_at = Some(Instant::now() + CLOSE_TIMEOUT);
                            }
                        }
                    }
                }
                _ = async {
                    if let Some(deadline) = pending_at {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if pending_at.is_some() => {
                    pending_at = flush_or_retry(event_tx, latest, &frame, &mut last_publish);
                }
                // 首屏只有图标、整块壁纸没画出来：按一次 F5 让远端资源管理器
                // 重画桌面。服务端偶尔在会话恢复时跳过壁纸绘制，图标却照常显示，
                // 从帧缓冲看就是「纯黑底 + 少量非黑像素」。只补一次，不反复打扰。
                _ = async {
                    if let Some(deadline) = wallpaper_check_at {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if wallpaper_check_at.is_some() => {
                    wallpaper_check_at = None;
                    let black = frame.as_ref().map_or(0.0, |(buffer, _, _)| {
                        let black = buffer
                            .iter()
                            .filter(|pixel| **pixel & 0x00FF_FFFF == 0)
                            .count();
                        black as f64 / buffer.len().max(1) as f64
                    });
                    if black > WALLPAPER_BLACK_THRESHOLD {
                        log::info!(
                            target: "ruterm::desktop::rdp",
                            "desktop looks black ({:.0}%), sending F5 to repaint wallpaper",
                            black * 100.0
                        );
                        let f5 = Scancode::from_u16(0x3F);
                        send_input(
                            &mut database,
                            &input,
                            [Operation::KeyPressed(f5), Operation::KeyReleased(f5)],
                        );
                    }
                }
                _ = async {
                    if let Some(deadline) = close_at {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if close_at.is_some() => {
                    // 服务器没在限时内完成优雅收尾：直接丢连接，但必须让 UI
                    // 知道会话已经结束，否则状态栏永远停在「正在断开远程桌面…」。
                    let _ = event_tx.send(Event::Closed {
                        reason: "RDP 连接关闭超时，已强制断开".to_string(),
                    });
                    return;
                }
                // 上传中的文件：远端确认文件列表后替用户按 Ctrl+V，同时回报进度。
                _ = async {
                    if let Some(deadline) = upload_tick {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if upload_tick.is_some() => {
                    let Some(state) = upload.as_mut() else {
                        upload_tick = None;
                        continue;
                    };
                    // 剪贴板线程已把这批文件报给远端，才有必要按键；否则远端剪贴板还是旧内容。
                    if state.progress.started()
                        && state
                            .paste_at
                            .is_some_and(|deadline| state.progress.acknowledged() || Instant::now() >= deadline)
                    {
                        state.paste_at = None;
                        send_input(&mut database, &input, paste_operations());
                    }

                    let done = state.progress.sent();
                    if done != state.last_done {
                        state.last_done = done;
                        state.last_change = Instant::now();
                    }
                    if state.started_at.is_none() && state.progress.started() {
                        state.started_at = Some(Instant::now());
                    }
                    // 远端开始读字节（或已回执）就算进入接收阶段：此后只看「还有没有新字节」。
                    let reading = state.progress.acknowledged() || done > 0;
                    if reading && !state.acked_seen {
                        state.acked_seen = true;
                        state.last_change = Instant::now();
                    }
                    if !state.progress.failed() {
                        let stalled = if state.started_at.is_none() {
                            // 还在枚举目录：大文件夹可能很久，别拿建连超时误判。
                            state.since.elapsed() >= UPLOAD_ENUM_TIMEOUT
                        } else if done == 0 {
                            // 远端收下列表后可能弹了覆盖确认框：等用户点完，别急着判失败。
                            state.last_change.elapsed() >= UPLOAD_START_TIMEOUT
                        } else {
                            state.last_change.elapsed() >= UPLOAD_IDLE_TIMEOUT
                        };
                        // If enumeration never attached our file list, `paste_at` is never
                        // cleared; the enumeration timeout must still finish the operation.
                        if stalled && (state.started_at.is_none() || state.paste_at.is_none()) {
                            state.progress.abort();
                        }
                    }

                    let failed = state.progress.failed();
                    let reportable = state.progress.started() || failed;
                    if reportable
                        && (done != state.reported_done || failed != state.reported_failed)
                    {
                        state.reported_done = done;
                        state.reported_failed = failed;
                        let _ = event_tx.send(Event::Upload {
                            done,
                            total: state.progress.total(),
                            files: state.progress.files(),
                            finished: state.progress.finished(),
                            failed,
                        });
                    }
                    if state.progress.finished() {
                        upload = None;
                        upload_tick = None;
                    } else {
                        upload_tick = Some(Instant::now() + UPLOAD_TICK);
                    }
                }
            }
        }
    });
    Ok(())
}

/// 一次拖放上传的界面侧跟踪状态。
struct UploadState {
    progress: Arc<UploadProgress>,
    /// 还没替用户按 Ctrl+V（等远端确认文件列表，或兜底超时）。
    paste_at: Option<Instant>,
    since: Instant,
    last_done: u64,
    last_change: Instant,
    /// 剪贴板线程开始上报这批文件的时间（枚举完目录才有）。
    started_at: Option<Instant>,
    /// 已经看到远端回执或实际读取字节。
    acked_seen: bool,
    reported_done: u64,
    reported_failed: bool,
}

impl UploadState {
    fn new(progress: Arc<UploadProgress>) -> Self {
        let now = Instant::now();
        Self {
            progress,
            paste_at: Some(now + PASTE_FALLBACK),
            since: now,
            last_done: 0,
            last_change: now,
            started_at: None,
            acked_seen: false,
            // 首轮进度一定要发一次，界面才知道要画卡片。
            reported_done: u64::MAX,
            reported_failed: false,
        }
    }
}

/// 一条服务器输出对会话的影响。
enum Output {
    /// 无需推帧（指针、心跳等）。
    Alive,
    /// 画面已更新，按节流规则决定何时发布。
    Dirty,
    /// 连接结束。
    Ended,
}

/// 把 `frame` 里最新整帧转成快照发布；返回是否真的发布成功。
fn flush_frame(
    event_tx: &UnboundedSender<Event>,
    latest: &Mutex<Option<Snapshot>>,
    frame: &Option<Frame>,
    last_publish: &mut Instant,
) -> bool {
    *last_publish = Instant::now();
    if let Some(current) = frame.as_ref()
        && publish_frame(latest, current)
    {
        let _ = event_tx.send(Event::FrameReady);
        return true;
    }
    false
}

/// 尝试发布最新整帧；返回下一次需要重试的时间（`None` 表示无需重试）。
///
/// 界面还没取走上一帧时槽位是占着的，这次转换会直接跳过；隔一拍再试，
/// 保证画面停止变化时最新一帧最终一定能送到界面。
fn flush_or_retry(
    event_tx: &UnboundedSender<Event>,
    latest: &Mutex<Option<Snapshot>>,
    frame: &Option<Frame>,
    last_publish: &mut Instant,
) -> Option<Instant> {
    if flush_frame(event_tx, latest, frame, last_publish) || frame.is_none() {
        None
    } else {
        Some(Instant::now() + FRAME_INTERVAL)
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
    wallpaper_check_at: &mut Option<Instant>,
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
                // 首帧到达后再等一小会儿检查壁纸：期间服务器还会补发增量，
                // 太早判断会把正常桌面误判成黑底。
                *wallpaper_check_at = Some(Instant::now() + WALLPAPER_CHECK_DELAY);
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
            // IronRDP 的 Error Display 只打印 `[context] kind`，真正的底层原因
            // （如 Connection refused / 主机不可达）挂在 source 链上，必须用
            // report() 展开，否则用户只看到「custom error」无从排查。
            let _ = event_tx.send(Event::Closed {
                reason: error.report().to_string(),
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

/// 把整屏 `0x00RRGGBB` 拷成界面用的 BGRA（高字节补 0xFF）。
///
/// 小端下 `0x00RRGGBB` 在内存里正好是 B,G,R,0，拷贝和补 alpha 合成一趟，
/// 1080p 一帧比「先 memcpy 再遍历补字节」少读一整块内存。这一步和解码共用
/// 会话线程，x86_64 用 SSE2 一次处理 4 个像素。
#[cfg(all(target_endian = "little", target_arch = "x86_64"))]
fn to_bgra(buffer: &[u32]) -> Vec<u8> {
    use std::arch::x86_64::{_mm_loadu_si128, _mm_or_si128, _mm_set1_epi32, _mm_storeu_si128};

    let mut bgra = Vec::<u8>::with_capacity(buffer.len() * 4);
    // SAFETY: x86_64 基线含 SSE2，无需运行时探测；下面按整像素写满
    // `len * 4` 字节后才 set_len，u8 无需析构，中间不会观察未初始化内存。
    unsafe {
        let alpha = _mm_set1_epi32(0xFF00_0000u32 as i32);
        let src = buffer.as_ptr().cast::<u8>();
        let dst = bgra.as_mut_ptr();
        let mut i = 0;
        while i + 4 <= buffer.len() {
            let pixels = _mm_loadu_si128(src.add(i * 4).cast());
            _mm_storeu_si128(dst.add(i * 4).cast(), _mm_or_si128(pixels, alpha));
            i += 4;
        }
        while i < buffer.len() {
            std::ptr::write_unaligned(dst.add(i * 4).cast::<u32>(), buffer[i] | 0xFF00_0000);
            i += 1;
        }
        bgra.set_len(buffer.len() * 4);
    }
    bgra
}

#[cfg(not(all(target_endian = "little", target_arch = "x86_64")))]
fn to_bgra(buffer: &[u32]) -> Vec<u8> {
    buffer
        .iter()
        .flat_map(|pixel| (*pixel | 0xFF00_0000).to_le_bytes())
        .collect()
}

/// 把整屏转成 BGRA 快照；界面还没取走上一帧就丢掉这次转换。
///
/// 转换在锁外完成：界面线程取快照时只拿一下锁，不会被这几 MB 的搬运挡在后面。
fn publish_frame(latest: &Mutex<Option<Snapshot>>, frame: &Frame) -> bool {
    // 槽位非空说明界面还没取走上一帧，这次连转换都省掉。
    match latest.lock() {
        Ok(slot) if slot.is_none() => {}
        _ => return false,
    }
    let (buffer, width, height) = frame;
    let bgra = to_bgra(buffer);

    let Ok(mut slot) = latest.lock() else {
        return false;
    };
    // 槽位只有本函数写；转换期间界面只会把它取空，这里照旧确认一次。
    if slot.is_some() {
        return false;
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
    held: &mut HeldInput,
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
            held.track(key, down);
            if let Some(operation) = key_operation(key, down) {
                send_input(database, input, [operation]);
            }
            false
        }
        Command::ReleaseAll => {
            held.release_all(database, input);
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
        // 剪贴板由 WinClipboard 后端直接挂系统剪贴板（文本与文件都自动双向同步）；
        // 界面侧再发一遍会覆盖远端刚复制的内容，这里忽略。
        Command::Clipboard(_) => false,
        // 文件拖放在会话循环里先进剪贴板、再按 Ctrl+V，不走这里。
        Command::PasteFiles(_) => false,
        Command::CancelUpload => false,
        // RDP 只在画面变化时推帧；界面要整帧时把最后一帧再放回槽位。
        Command::Refresh | Command::RequestFull => {
            if let Some(frame) = frame {
                publish_frame(latest, frame);
            }
            false
        }
    }
}

/// 会话线程记住已按下的 Unicode 字符。扫描码键由 `Database` 自己记账，
/// 界面在 Esc、失焦或关闭时发 `ReleaseAll`，把两边都清干净，避免远端一直
/// 以为某个键还按着（切走窗口后最常见的“黏键”）。
#[derive(Default)]
struct HeldInput {
    unicode: std::collections::BTreeSet<char>,
}

impl HeldInput {
    fn track(&mut self, key: KeyInput, down: bool) {
        let Some(character) = key.ch.filter(|_| key.scancode.is_none()) else {
            return;
        };
        if down {
            self.unicode.insert(character);
        } else {
            self.unicode.remove(&character);
        }
    }

    fn release_all(&mut self, database: &mut Database, input: &UnboundedSender<RdpInputEvent>) {
        let mut operations = Vec::new();
        for index in 0..512u16 {
            let scancode = (index & 0x100 != 0, (index & 0xFF) as u8);
            let scancode = Scancode::from(scancode);
            if database.is_key_pressed(scancode) {
                operations.push(Operation::KeyReleased(scancode));
            }
        }
        for character in std::mem::take(&mut self.unicode) {
            operations.push(Operation::UnicodeKeyReleased(character));
        }
        for button in [
            MouseButton::Left,
            MouseButton::Middle,
            MouseButton::Right,
            MouseButton::X1,
            MouseButton::X2,
        ] {
            if database.is_mouse_button_pressed(button) {
                operations.push(Operation::MouseButtonReleased(button));
            }
        }
        send_input(database, input, operations);
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
    // 先移动再按键：按下/释放必须落在本次事件携带的坐标上。按钮过渡排在
    // MouseMove 之前的话，服务器会在"上一个已知光标位置"处理点击，首击
    // 会点错地方。Wheel 路径已经是这个顺序。
    operations.push(Operation::MouseMove(MousePosition { x, y }));
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

/// Ctrl+V：文件列表到远端后，由资源管理器把文件粘到当前文件夹。
fn paste_operations() -> [Operation; 4] {
    let ctrl = Scancode::from_u16(0x1D);
    let v = Scancode::from_u16(0x2F);
    [
        Operation::KeyPressed(ctrl),
        Operation::KeyPressed(v),
        Operation::KeyReleased(v),
        Operation::KeyReleased(ctrl),
    ]
}

/// 把文件列表放进系统剪贴板（`CF_HDROP`）：剪贴板线程的监听窗口会把它报给远端。
///
/// 只做本机剪贴板写入，不需要 OLE；剪贴板线程负责发现这次变化。
#[cfg(windows)]
fn set_clipboard_files(paths: &[PathBuf]) -> bool {
    use std::os::windows::ffi::OsStrExt as _;

    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::DROPFILES;

    if paths.is_empty() {
        return false;
    }
    // DROPFILES 头 + 每项一个 \0，末尾再补一个 \0 结束列表。
    let mut names: Vec<u16> = Vec::new();
    for path in paths {
        names.extend(path.as_os_str().encode_wide());
        names.push(0);
    }
    names.push(0);
    let header = std::mem::size_of::<DROPFILES>();
    let bytes = header + names.len() * std::mem::size_of::<u16>();

    // SAFETY: 剪贴板在 OpenClipboard / CloseClipboard 之间独占；写入的 HGLOBAL
    // 成功交给系统后不再由本进程释放。
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let result = (|| -> windows::core::Result<()> {
            EmptyClipboard()?;
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
            let base = GlobalLock(memory);
            if base.is_null() {
                let _ = GlobalFree(Some(memory));
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::write(
                base.cast::<DROPFILES>(),
                DROPFILES {
                    pFiles: u32::try_from(header).expect("DROPFILES 头长度"),
                    pt: Default::default(),
                    fNC: false.into(),
                    fWide: true.into(),
                },
            );
            std::ptr::copy_nonoverlapping(
                names.as_ptr(),
                base.cast::<u16>().add(header / 2),
                names.len(),
            );
            let _ = GlobalUnlock(memory);
            if SetClipboardData(u32::from(CF_HDROP.0), Some(HANDLE(memory.0))).is_err() {
                let _ = GlobalFree(Some(memory));
                return Err(windows::core::Error::from_thread());
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result.is_ok()
    }
}

/// 非 Windows 没有系统剪贴板文件通道；RDP 文件粘贴只在 Windows 生效。
#[cfg(not(windows))]
fn set_clipboard_files(_paths: &[PathBuf]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_frame_writes_opaque_bgra() {
        let latest = Mutex::new(None);
        // 0x00RRGGBB = 红 0x11、绿 0x22、蓝 0x33。
        let frame = (vec![0x0011_2233], 1, 1);
        assert!(publish_frame(&latest, &frame));

        let snapshot = latest.lock().unwrap().take().expect("快照已发布");
        assert_eq!((snapshot.width, snapshot.height), (1, 1));
        assert_eq!(snapshot.bgra, vec![0x33, 0x22, 0x11, 0xFF]);
    }

    #[test]
    fn publish_frame_skips_when_slot_is_busy() {
        let latest = Mutex::new(Some(Snapshot {
            width: 1,
            height: 1,
            bgra: vec![0; 4],
        }));
        assert!(!publish_frame(&latest, &(vec![0x0011_2233], 1, 1)));
    }

    #[test]
    fn client_config_advertises_only_remotefx() {
        // 守住图形路径：只广告服务端真正支持的 RemoteFX，避免将来打开 qoi/qoiz
        // feature 后悄悄改变协商结果（不同编解码器的首屏行为并不一致）。
        let config = Config {
            protocol: crate::desktop::Protocol::Rdp,
            host: "127.0.0.1".to_owned(),
            port: 3389,
            password: None,
            shared: false,
            username: Some("tester".to_owned()),
            domain: None,
            size: (1280, 800),
        };
        let client = build_client_config(&config).expect("配置有效");
        let codecs = &client
            .connector()
            .bitmap
            .as_ref()
            .expect("bitmap 配置存在")
            .codecs
            .0;
        assert_eq!(codecs.len(), 1, "只应广告 RemoteFX 一种编解码器");
        // CODEC_ID_REMOTEFX == 3；QOI(0x0A)/QOIZ(0x0B) 都不应在列。
        assert_eq!(codecs[0].id, 3);
    }

    #[test]
    fn to_bgra_converts_simd_blocks_and_tail() {
        // 7 个像素：SSE2 一次 4 个，剩下的零头走逐像素分支。
        let bgra = to_bgra(&[0x0011_2233; 7]);
        assert_eq!(bgra.len(), 7 * 4);
        for pixel in bgra.as_chunks::<4>().0 {
            assert_eq!(pixel, &[0x33, 0x22, 0x11, 0xFF]);
        }
    }
}
