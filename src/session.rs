//! 会话桥接：rgpui-term 外接 IO 后端 ⇄ russh 通道的双向字节流。
//!
//! - 下行：`SshTerminalHandler::data` → `FeedHandle::feed`（见 ssh.rs）
//! - 上行：终端阻塞通道（[`PtyBoundReceiver`]）→ 专用泵线程 →
//!   tokio 无界通道 → 转发任务写入 russh 通道（按键/应答/尺寸变化）
//!
//! 网络 IO 全部在 rgpui 的 tokio 运行时上异步进行，UI 线程零阻塞。

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use rgpui::AppContext;
use rgpui::Task;
use rgpui_term::{ExternalHandles, FeedHandle, PtyBoundMessage};
use russh::Channel;
use russh::ChannelMsg;
use russh::ChannelWriteHalf;
use russh::client::{Config as SshConfig, Handle, Msg};
use russh::keys::PrivateKeyWithHashAlg;
use russh::keys::agent::client::AgentClient;
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::config::SshTarget;
use crate::remote::{PROBE_CMD, RemoteInfo};
use crate::ssh::{ForwardedTcpip, SideChannels, SshTerminalHandler};
use crate::transfer;

/// 信号：私钥有口令但连接未携带口令。
///
/// UI 层据此弹出口令输入窗，而不是把加载失败当普通错误一笔带过。
#[derive(Debug)]
pub struct PassphraseRequired;

impl std::fmt::Display for PassphraseRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("私钥已加密，需要口令")
    }
}

impl std::error::Error for PassphraseRequired {}

/// 信号：本次连接使用的秘密（密码/私钥口令/Agent 密钥）被拒绝或已失效。
///
/// UI 层据此清除内存缓存；若秘密来自加密库，同时清除库内条目，
/// 下次点击会重新弹窗输入，而不是拿坏密码反复直连。
#[derive(Debug)]
pub struct SecretRejected;

impl std::fmt::Display for SecretRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("认证被拒绝")
    }
}

impl std::error::Error for SecretRejected {}

/// 远端 PTY 的初始尺寸（终端首个 Resize 到达前生效）。
const INIT_COLS: u16 = 80;
const INIT_ROWS: u16 = 24;

/// 远程信息首次探测的超时：远端禁用 exec 或命令挂死时放弃，状态栏不显示该项。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// 远程信息刷新间隔（负载/内存/磁盘随会话持续变化，只探一次会一直显示连接时的值）。
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// 客户端会话通道的写半边（上行方向）。
type SessionChannelWrite = ChannelWriteHalf<Msg>;

/// 转发方向：本地转发（-L）监听本机，远程转发（-R）让服务器监听，
/// 动态转发（-D）在本机开 SOCKS5 代理、目标由客户端逐条协商。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    /// `ssh -L`：本机 `127.0.0.1:<listen_port>` → 服务器视角的 `target_host:target_port`。
    Local,
    /// `ssh -R`：服务器 `127.0.0.1:<listen_port>` → 本机视角的 `target_host:target_port`。
    Remote,
    /// `ssh -D`：本机 `127.0.0.1:<listen_port>` 上的 SOCKS5 代理，目标由客户端请求决定。
    Dynamic,
}

impl ForwardKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Local => "L",
            Self::Remote => "R",
            Self::Dynamic => "D",
        }
    }
}

/// 一条端口转发规则。
///
/// 三个方向共用同一组字段，语义随 `kind` 变化：
/// - 本地转发：`listen_port` 是本机端口，目标是服务器视角的地址；
/// - 远程转发：`listen_port` 是服务器端口（只绑服务器回环），目标是本机视角的地址；
/// - 动态转发：`listen_port` 是本机 SOCKS5 端口，目标字段忽略（由客户端握手给出）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardRule {
    pub kind: ForwardKind,
    pub listen_port: u16,
    pub target_host: String,
    pub target_port: u16,
}

impl ForwardRule {
    /// 弹窗与状态栏展示用的一行描述。
    pub fn label(&self) -> String {
        match self.kind {
            ForwardKind::Local => format!(
                "L 127.0.0.1:{} → {}:{}",
                self.listen_port, self.target_host, self.target_port
            ),
            ForwardKind::Remote => format!(
                "R 服务器:{} → 本机 {}:{}",
                self.listen_port, self.target_host, self.target_port
            ),
            ForwardKind::Dynamic => {
                format!("D 127.0.0.1:{}（SOCKS5）", self.listen_port)
            }
        }
    }

    /// 规则列表里的唯一 id（方向 + 端口，各方向端口可重合）。
    pub fn id(&self) -> String {
        format!("{}-{}", self.kind.prefix(), self.listen_port)
    }
}

/// 本地监听 accept 到的一条待转发连接。
///
/// 通道必须在持有 `Handle` 的会话任务里打开（`Handle` 不可克隆、也不是 Sync），
/// 所以 accept 任务只负责把连接（以及 SOCKS5 已协商出的目标）递进来。
enum AcceptedForward {
    /// 本地转发：目标在规则里写死。
    Direct {
        rule: ForwardRule,
        socket: TcpStream,
        peer_port: u16,
    },
    /// 动态转发：SOCKS5 握手已完成，目标来自客户端请求。
    Socks {
        rule: ForwardRule,
        socket: TcpStream,
        target_host: String,
        target_port: u16,
        peer_port: u16,
    },
}

/// 一条生效中的转发：规则 + 监听任务与已建立连接的双向泵。
///
/// 停止转发要连活动连接一起掐断，不能让"停止"只关监听、流量照旧。
struct ActiveForward {
    rule: ForwardRule,
    // 监听、SOCKS 握手和数据泵同属此集合，停止规则时一并取消并等待。
    pumps: JoinSet<()>,
}

/// 框架接收端没有可取消的阻塞 recv。这里只轮询上行，不把定时器当作下行背压。
/// 该任务必须放进会话 JoinSet，关闭标签无需等待 Terminal 实体的所有引用释放。
pub(crate) async fn pump_external(
    rx: rgpui_term::PtyBoundReceiver,
    tx: mpsc::UnboundedSender<PtyBoundMessage>,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = tx.closed() => return,
            _ = tick.tick() => {
                for _ in 0..64 {
                    let Some(message) = rx.try_recv() else { break };
                    if tx.send(message).is_err() { return; }
                }
            }
        }
    }
}

/// UI 发给会话任务的指令：SFTP 与端口转发都必须借用同一条已认证的 SSH 连接。
pub enum SessionCommand {
    /// 读取远端主目录，用于文件传输弹窗预填路径。
    Home {
        reply: oneshot::Sender<Result<String>>,
    },
    /// 文件面板"跟随终端"：确认某路径是远端目录（用于校验 cd 猜测与 OSC 7 上报）。
    FsStat {
        path: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    /// 执行一次上传 / 下载。
    Transfer(transfer::Task),
    /// 文件面板：列出目录（None / `~` 表示登录主目录）。
    FsList {
        path: Option<String>,
        reply: oneshot::Sender<Result<crate::remote_fs::Listing>>,
    },
    /// 文件面板：在 `dir` 下新建目录，回传新目录完整路径。
    FsMkdir {
        dir: String,
        name: String,
        reply: oneshot::Sender<Result<String>>,
    },
    /// 文件面板：重命名 `dir` 下的条目，回传新路径。
    FsRename {
        dir: String,
        from: String,
        to: String,
        reply: oneshot::Sender<Result<String>>,
    },
    /// 文件面板：删除文件或空目录。
    FsDelete {
        path: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// 拖文件到终端：在 shell 当前目录用 `rz`（lrzsz）接收该文件。
    ZmodemUpload {
        local: std::path::PathBuf,
        name: String,
        progress: mpsc::UnboundedSender<(u64, u64)>,
        reply: oneshot::Sender<Result<transfer::Summary>>,
    },
    /// 取消进行中的 lrzsz 上传：置位取消标记、摘掉下行接管点并往远端发
    /// Ctrl+C。没有进行中的传输时也安全（no-op）。
    ZmodemCancel {
        reply: oneshot::Sender<()>,
    },
    /// 开始一条转发（本机监听，或请求服务器监听）。
    ///
    /// 回传生效规则：远程转发请求端口 0 时服务器会分配实际端口。
    ForwardStart {
        rule: ForwardRule,
        reply: oneshot::Sender<Result<ForwardRule>>,
    },
    /// 停止一条转发（按方向 + 监听端口定位；未在转发时回 false）。
    ForwardStop {
        rule: ForwardRule,
        reply: oneshot::Sender<bool>,
    },
}

/// 终端目录线索：文件面板"跟随终端"的输入源。
#[derive(Debug)]
pub enum CwdHint {
    /// 从键入流里识别出的 cd / pushd 参数（空串＝回主目录）。
    Cd(String),
    /// 远端 shell 通过 OSC 7 上报的绝对路径（已百分号解码）。
    Report(String),
}

/// 一次 SSH 会话：连接主体 + 远程基本信息回传 + 启动标记。
pub struct SshSession {
    /// 会话任务，必须被持有（提前 drop 会取消 SSH）。
    pub join: Task<Result<()>>,
    /// 远程主机基本信息流（连接期间每 [`PROBE_INTERVAL`] 刷新一次；探测失败时无数据）。
    pub remote_info: mpsc::UnboundedReceiver<RemoteInfo>,
    /// 终端工作目录确认流：仅在 SFTP 校验"确为目录"后回传（跟随时面板据此跳转）。
    pub cwd_events: mpsc::UnboundedReceiver<String>,
    /// shell 是否成功开启。失败发生在开启之前＝没建立过终端，标签页应保留展示错误。
    pub started: Arc<AtomicBool>,
    /// shell 成功开启时发送一次：UI 据此记录"连接真正可用"的时刻（判断会话是否稳定过）。
    pub ready: oneshot::Receiver<()>,
    /// 主动断开信号：标签被关闭时发送，会话任务随即释放连接。
    pub disconnect: oneshot::Sender<()>,
    /// 会话指令通道（SFTP / 端口转发；本地终端没有会话，不持有）。
    pub commands: mpsc::UnboundedSender<SessionCommand>,
}

/// anyhow 错误链拍平成一行（状态栏与终端展示用，避免只看到顶层语境）。
pub fn error_summary(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("：")
}

/// 在 rgpui 的 tokio 运行时上启动 SSH 会话。
///
/// 返回的任务必须被持有（提前 drop 会取消 SSH）。
/// `vault` 用于加密库内的 known_hosts 校验；None 时回退旧文件（降级模式）。
/// `connect_timeout` 覆盖"建连 → 认证 → 开 shell"整个阶段。
pub fn start<C: AppContext>(
    target: SshTarget,
    handles: ExternalHandles,
    vault: Option<Arc<crate::vault::Vault>>,
    connect_timeout: Duration,
    shell_integration: bool,
    cx: &C,
) -> SshSession {
    let ExternalHandles { feed, rx } = handles;
    // 上行泵：终端的阻塞接收端只在本任务里轮询，关标签后发送端关闭，
    // 任务随之退出；放进会话任务里，提前取消时不会留下卡在 recv 的线程。
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<PtyBoundMessage>();
    let uplink = rgpui::tokio::Tokio::spawn(cx, pump_external(rx, ui_tx));
    let (info_tx, info_rx) = mpsc::unbounded_channel::<RemoteInfo>();
    let (cwd_tx, cwd_rx) = mpsc::unbounded_channel::<String>();
    let (ready_tx, ready_rx) = oneshot::channel::<()>();
    let (disconnect_tx, disconnect_rx) = oneshot::channel::<()>();
    let (command_tx, command_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let started = Arc::new(AtomicBool::new(false));

    feed.feed(format!("\r\n[ruterm] 正在连接 {}…\r\n", target.addr()).into_bytes());

    let closing_feed = feed.clone();
    let started_flag = started.clone();
    // 会话自己留一个发送端：UI 关掉所有标签后通道不会立刻变空，
    // 选择循环里的 `recv()` 保持挂起而不是每秒空转。
    let command_keepalive = command_tx.clone();
    let join = rgpui::tokio::Tokio::spawn_result(cx, async move {
        let result = run(
            target,
            feed,
            ui_rx,
            info_tx,
            cwd_tx,
            started_flag,
            vault,
            disconnect_rx,
            command_rx,
            command_keepalive,
            connect_timeout,
            shell_integration,
            ready_tx,
        )
        .await;
        // run 结束后不再需要上行泵：丢弃 Task 即取消。
        drop(uplink);
        if let Err(error) = &result {
            closing_feed
                .feed(format!("\r\n[ruterm] 会话结束: {}\r\n", error_summary(error)).into_bytes());
        }
        // 无论成功失败都以"远端退出"收场，UI 表现与本地 shell 退出一致。
        closing_feed.end_session();
        result
    });
    SshSession {
        join,
        remote_info: info_rx,
        cwd_events: cwd_rx,
        started,
        ready: ready_rx,
        disconnect: disconnect_tx,
        commands: command_tx,
    }
}

/// 建连 → 认证 → 开 PTY/shell → 探测远程信息 → 驱动会话直至断线。
#[allow(clippy::too_many_arguments)]
async fn run(
    target: SshTarget,
    feed: FeedHandle,
    ui_rx: mpsc::UnboundedReceiver<PtyBoundMessage>,
    info_tx: mpsc::UnboundedSender<RemoteInfo>,
    cwd_tx: mpsc::UnboundedSender<String>,
    started: Arc<AtomicBool>,
    vault: Option<Arc<crate::vault::Vault>>,
    mut disconnect: oneshot::Receiver<()>,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    command_keepalive: mpsc::UnboundedSender<SessionCommand>,
    connect_timeout: Duration,
    shell_integration: bool,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    // 终端目录线索：上行键入解析（cd 参数）与下行 OSC 7 上报汇入同一条队列，
    // 由校验任务逐条经 SFTP 确认后再回传 UI，猜测错误不会污染面板。
    let (cwd_hint_tx, cwd_hint_rx) = mpsc::unbounded_channel::<CwdHint>();
    // 远程转发的通道由 Handler 接住后递到这里：会话循环统一拨本机目标并搬运字节。
    let (forwarded_tx, mut forwarded_rx) = mpsc::unbounded_channel::<ForwardedTcpip>();
    // lrzsz 上传的接管点与原始上行通道：接管期间终端输出改投 ZMODEM 状态机，
    // 原始上行绕过终端 IO 直接写通道（`rz` 命令与协议帧都不能经过行编辑）。
    let zmodem_sink = crate::ssh::zmodem_sink();
    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // 建连 → 认证 → 开 shell 整体限时：服务器不响应时应明确报超时，
    // 而不是让标签永远停在"正在连接"（拖住自动重连的退避节奏）。
    let setup = async {
        let side = SideChannels::default();
        // Handler 在交互通道 EOF/close/exit-status 时发信号：会话到此为止，
        // 不能继续等 `handle`（TCP 连接还在，会一直挂着，UI 看起来毫无反应）。
        let (ended_tx, ended_rx) = mpsc::unbounded_channel::<()>();

        // 跳板机链：从最外层开始逐级建连，每一跳用 direct-tcpip 打洞到下一跳，
        // 目标 SSH 跑在该通道之上，因此跳板机不需要任何转发规则（只需 sshd 允许转发）。
        let chain = target.jump_chain();
        if !chain.is_empty() {
            let path = chain
                .iter()
                .map(SshTarget::addr)
                .chain(std::iter::once(target.addr()))
                .collect::<Vec<_>>()
                .join(" → ");
            feed.feed(format!("\r\n[ruterm] 经跳板机连接：{path}\r\n").into_bytes());
        }
        // 跳板机自己的通道结束 / forwarded 信号与主会话无关：接收端马上丢弃，
        // 发送失败由 `let _ =` 吞掉。
        let (jump_ended_tx, _jump_ended_rx) = mpsc::unbounded_channel::<()>();
        let (jump_forwarded_tx, _jump_forwarded_rx) = mpsc::unbounded_channel::<ForwardedTcpip>();

        // 已建立的跳板机连接必须活到会话结束（句柄一丢，通道随之关闭），
        // 所以收集在这里、最后连同主会话句柄一起带出 setup。
        let mut hops: Vec<(Handle<SshTerminalHandler>, SideChannels)> = Vec::new();
        // 终端通道登记：direct-tcpip 旁路「先说话」的字节不灌进终端。
        let terminal_channel = crate::ssh::TerminalChannel::default();
        let nodes: Vec<&SshTarget> = chain.iter().chain(std::iter::once(&target)).collect();
        for (index, node) in nodes.iter().enumerate() {
            let is_target = index + 1 == nodes.len();
            // keepalive：NAT / 断网这类"静默断连"不会触发 TCP EOF，is_closed() 永远
            // 不会翻转，会话僵死、自动重连也永不触发。20s 一次 keepalive，3 次无响应
            // 由 russh 主动关闭连接，健康检查就能上报并走重连。
            // nodelay：终端逐键上行的场景下 Nagle 会把下一次按键拖到上一个包被确认
            // 之后（最多一个 RTT），telnet 侧早已 set_nodelay(true)，SSH 侧对齐。
            let config = Arc::new(SshConfig {
                keepalive_interval: Some(std::time::Duration::from_secs(20)),
                nodelay: true,
                ..SshConfig::default()
            });
            let node_side = if is_target {
                side.clone()
            } else {
                SideChannels::default()
            };
            // 终端通道登记只有目标连接有意义（跳板的 feed 不渲染）：跳板各自
            // 用一个空的登记，永远不会命中。
            let handler = SshTerminalHandler::new(
                feed.clone(),
                node.host.clone(),
                node.port,
                node_side.clone(),
                vault.clone(),
                if is_target {
                    ended_tx.clone()
                } else {
                    jump_ended_tx.clone()
                },
                if is_target {
                    forwarded_tx.clone()
                } else {
                    jump_forwarded_tx.clone()
                },
                if is_target {
                    Some(zmodem_sink.clone())
                } else {
                    None
                },
                if is_target {
                    Some(cwd_hint_tx.clone())
                } else {
                    None
                },
                if is_target {
                    terminal_channel.clone()
                } else {
                    // 跳板机连接没有终端：非旁路字节（下一跳 sshd 的 banner
                    // 等）一概丢弃，不再可能漏进共享的终端 feed。
                    let jump_terminal = crate::ssh::TerminalChannel::default();
                    jump_terminal.disable();
                    jump_terminal
                },
            );
            let hop = if index == 0 {
                String::new()
            } else {
                nodes[index - 1].addr()
            };
            let mut handle = match hops.last_mut() {
                None => russh::client::connect(config, (node.host.as_str(), node.port), handler)
                    .await
                    .with_context(|| format!("无法连接 {}", node.addr()))?,
                Some((previous, previous_side)) => {
                    // 用上一跳的 connected 地址（跳板机视角）开 direct-tcpip，
                    // 目标 SSH 字节流全部跑在这条通道里。
                    let channel = previous
                        .channel_open_direct_tcpip(
                            node.host.clone(),
                            u32::from(node.port),
                            "127.0.0.1",
                            0,
                        )
                        .await
                        .with_context(|| format!("跳板机 {hop} 无法转发到 {}", node.addr()))?;
                    // 通道必须登记进跳板机的旁路集合，否则字节会灌进终端。
                    previous_side.add(channel.id());
                    russh::client::connect_stream(config, channel.into_stream(), handler)
                        .await
                        .with_context(|| format!("经跳板机 {hop} 连接 {} 失败", node.addr()))?
                }
            };
            if let Err(error) = authenticate(&mut handle, node).await {
                if is_target {
                    return Err(error);
                }
                return Err(error.context(format!("跳板机 {} 认证失败", node.addr())));
            }
            hops.push((handle, node_side));
        }

        let (handle, _) = hops.pop().expect("链上至少有最终目标一个节点");
        let channel = handle.channel_open_session().await?;
        // shell 通道打开即登记为终端通道：旁路通道（转发 / 跳板）「先说话」的
        // 字节不再可能灌进终端（direct-tcpip 登记 side 之前存在空窗）。
        terminal_channel.set(channel.id());
        // 声明 locale：vim 等按 locale 决定内部编码，POSIX 会退成 latin1，
        // 重绘时把多字节字符拆散穿插转义序列，任何 UTF-8 终端都显示成乱码。
        // 对齐 OpenSSH 的 `SendEnv LANG LC_*`：本地显式设置就透传；没有则默认
        // 声明 C.UTF-8。sshd 未配置 AcceptEnv 时会静默拒绝，不影响连接。
        for (name, value) in ssh_locale_env() {
            let _ = channel.set_env(false, name, value).await;
        }
        channel
            .request_pty(
                true,
                "xterm-256color",
                u32::from(INIT_COLS),
                u32::from(INIT_ROWS),
                0,
                0,
                &[],
            )
            .await?;
        channel.request_shell(true).await?;
        started.store(true, Ordering::SeqCst);
        let _ = ready.send(());
        Ok::<_, anyhow::Error>((handle, channel, side, ended_rx, hops))
    };
    let (handle, channel, side, mut ended_rx, _jump_hops) =
        match tokio::time::timeout(connect_timeout, setup).await {
            Ok(result) => result?,
            Err(_) => anyhow::bail!(
                "连接超时（{} 秒内未完成握手、认证或开启 shell）",
                connect_timeout.as_secs()
            ),
        };

    // 终端下行由 `SshTerminalHandler::data` 直接喂给终端；russh 仍会把每条
    // CHANNEL_DATA 复制一份塞进通道自带的有界接收队列（默认 100 条）。该队列
    // 没人消费，满 100 条后会话任务永久阻塞在 `chan.send().await`，上下行一起
    // 停摆（终端看似卡死）。读半边若丢弃，send 会立即失败返回，不再堵住会话。
    let (channel_read, channel) = channel.split();
    drop(channel_read);

    // 上行转发先落地：探测通道握手即使被慢服务器拖住，也不能延迟按键转发。
    // last_input 记录用户最后一次敲键时刻：shell 集成注入靠它避开正在输入的人。
    let last_input: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    tokio::spawn(forward_uplink(
        channel,
        ui_rx,
        raw_rx,
        cwd_hint_tx,
        last_input.clone(),
    ));

    // 目录线索校验：cd 猜测与 OSC 7 上报都先经 SFTP 确认"确为目录"，
    // 只有确认成功才回传 UI 并推进本地跟踪状态（失败=cd 没成功，面板原地不动）。
    tokio::spawn(follow_cwd(cwd_hint_rx, command_keepalive, cwd_tx));

    probe_remote_info(&handle, side.clone(), info_tx).await;

    // 文件面板「跟随 / 定位」的目录来源：让远端 shell 自己用 OSC 7 上报 $PWD。
    // 只追加提示符钩子，失败或 shell 不认就原样留着按键解析兜底。
    if shell_integration {
        install_cwd_report(&handle, &side, &raw_tx, &last_input).await;
    }

    // 三条收场路径：远端通道结束、本地关标签、连接本身异常终止。
    // 连接是否还活着改为每秒查一次 `is_closed()`（`Handle` 作为 Future 消费后
    // 就不能再借用它开 SFTP 通道），前两者仍主动丢弃 handle 释放连接。
    // 端口转发的监听任务随会话一起收场：任一收场路径都必须 abort，否则
    // 本地端口会一直占着，用户重连后反而起不来。accept 到的连接在会话循环里
    // 开通道（Handle 不能克隆进任务），双向泵按转发归属统一收尾。
    // 本地 / 远程 / 动态转发分开记账：各自的「监听端口」含义不同。
    let mut local_forwards: HashMap<u16, ActiveForward> = HashMap::new();
    let mut remote_forwards: HashMap<u16, ActiveForward> = HashMap::new();
    let mut dynamic_forwards: HashMap<u16, ActiveForward> = HashMap::new();
    // 文件面板专用 SFTP：浏览 / 新建 / 重命名 / 删除共用一条子系统通道，
    // 不随每次点击重开（否则每次目录操作都要多一轮握手）。
    // 缓存放共享锁里：Fs* 命令的实际操作全部 spawn 到会话循环之外执行
    // （单请求内部超时 120 秒，大目录可能串行多轮），慢请求不能再堵住
    // disconnect / 转发派发 / 健康检查；开启子系统（≤10 秒）留在循环内，
    // 因为 Handle 不能克隆进任务。
    let fs_sftp: Arc<Mutex<Option<Arc<SftpSession>>>> = Arc::new(Mutex::new(None));
    let (accepted_tx, mut accepted_rx) = mpsc::unbounded_channel::<AcceptedForward>();
    let mut health = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = &mut disconnect => {
                log::info!("本地关闭会话 {}", target.addr());
                break;
            }
            _ = ended_rx.recv() => {
                log::info!("远端会话结束 {}", target.addr());
                break;
            }
            Some(command) = commands.recv() => {
                match command {
                    SessionCommand::Home { reply } => {
                        // open_sftp 自带 10 秒超时；remote_home 的单次请求内部
                        // 超时 120 秒——spawn 出去，别让慢服务器堵住会话循环。
                        match open_sftp(&handle, &side).await {
                            Ok(sftp) => {
                                tokio::spawn(async move {
                                    let home = transfer::remote_home(&sftp).await;
                                    let _ = sftp.close().await;
                                    let _ = reply.send(home);
                                });
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                    SessionCommand::Transfer(task) => {
                        match open_sftp(&handle, &side).await {
                            Ok(sftp) => {
                                let transfer::Task {
                                    direction,
                                    local,
                                    remote,
                                    progress,
                                    reply,
                                } = task;
                                tokio::spawn(async move {
                                    let result = transfer::execute(
                                        sftp, direction, local, remote, progress,
                                    )
                                    .await;
                                    let _ = reply.send(result);
                                });
                            }
                            Err(error) => {
                                let _ = task.reply.send(Err(error));
                            }
                        }
                    }
                    SessionCommand::FsList { path, reply } => {
                        match cached_sftp(&handle, &side, &fs_sftp).await {
                            Ok(sftp) => {
                                let cache = fs_sftp.clone();
                                tokio::spawn(async move {
                                    let result =
                                        crate::remote_fs::list(&sftp, path.as_deref()).await;
                                    if result.is_err() {
                                        drop_cached_sftp(&cache);
                                    }
                                    let _ = reply.send(result);
                                });
                            }
                            Err(error) => {
                                drop_cached_sftp(&fs_sftp);
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                    SessionCommand::FsStat { path, reply } => {
                        match cached_sftp(&handle, &side, &fs_sftp).await {
                            Ok(sftp) => {
                                let cache = fs_sftp.clone();
                                tokio::spawn(async move {
                                    let result = sftp
                                        .metadata(&path)
                                        .await
                                        .map(|meta| meta.is_dir())
                                        .context("无法读取远端路径信息");
                                    if result.is_err() {
                                        drop_cached_sftp(&cache);
                                    }
                                    let _ = reply.send(result);
                                });
                            }
                            Err(error) => {
                                drop_cached_sftp(&fs_sftp);
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                    SessionCommand::FsMkdir { dir, name, reply } => {
                        match cached_sftp(&handle, &side, &fs_sftp).await {
                            Ok(sftp) => {
                                let cache = fs_sftp.clone();
                                tokio::spawn(async move {
                                    let result = crate::remote_fs::mkdir(&sftp, &dir, &name).await;
                                    if result.is_err() {
                                        drop_cached_sftp(&cache);
                                    }
                                    let _ = reply.send(result);
                                });
                            }
                            Err(error) => {
                                drop_cached_sftp(&fs_sftp);
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                    SessionCommand::FsRename {
                        dir,
                        from,
                        to,
                        reply,
                    } => {
                        match cached_sftp(&handle, &side, &fs_sftp).await {
                            Ok(sftp) => {
                                let cache = fs_sftp.clone();
                                tokio::spawn(async move {
                                    let result =
                                        crate::remote_fs::rename(&sftp, &dir, &from, &to).await;
                                    if result.is_err() {
                                        drop_cached_sftp(&cache);
                                    }
                                    let _ = reply.send(result);
                                });
                            }
                            Err(error) => {
                                drop_cached_sftp(&fs_sftp);
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                    SessionCommand::FsDelete { path, reply } => {
                        match cached_sftp(&handle, &side, &fs_sftp).await {
                            Ok(sftp) => {
                                let cache = fs_sftp.clone();
                                tokio::spawn(async move {
                                    let result = crate::remote_fs::delete(&sftp, &path).await;
                                    if result.is_err() {
                                        drop_cached_sftp(&cache);
                                    }
                                    let _ = reply.send(result);
                                });
                            }
                            Err(error) => {
                                drop_cached_sftp(&fs_sftp);
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                    SessionCommand::ZmodemUpload {
                        local,
                        name,
                        progress,
                        reply,
                    } => {
                        // 下行接管与 `rz` 命令必须在同一个任务里成对安装 / 摘除，
                        // 否则 shell 提示符会被 ZMODEM 状态机吃掉。
                        // 接管点只有一个：并发第二条上传会覆盖第一条的 sink，
                        // 第一条结束时又把第二条摘掉、往新传输里注入 Ctrl+C，
                        // 两条一起报废——这里直接拒绝并发请求。
                        let mut sink_slot = zmodem_sink
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if sink_slot.is_some() {
                            let _ = reply.send(Err(anyhow::anyhow!(
                                "已有一条 ZMODEM 传输在进行，请等它结束"
                            )));
                            continue;
                        }
                        let (down_tx, mut down_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        *sink_slot = Some(crate::ssh::ZmodemTransfer {
                            downlink: down_tx.clone(),
                            cancel: cancel.clone(),
                        });
                        drop(sink_slot);
                        let sink = zmodem_sink.clone();
                        let raw = raw_tx.clone();
                        let feed = feed.clone();
                        tokio::spawn(async move {
                            let result = async {
                                raw.send(b"rz -y\r".to_vec())
                                    .map_err(|_| anyhow::anyhow!("会话上行通道已关闭"))?;
                                crate::zmodem::send_file(
                                    &local, &name, &mut down_rx, &raw, &progress, &cancel,
                                )
                                .await
                            }
                            .await;
                            // 只有接管点仍是本任务安装的通道时才摘除：若已被新的
                            // 上传替换，不能动它，更不能往新传输里发 Ctrl+C。
                            let owns_sink = sink
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .as_ref()
                                .map(|transfer| {
                                    transfer.downlink.same_channel(&down_tx)
                                        && !transfer.cancel.load(std::sync::atomic::Ordering::Relaxed)
                                })
                                .unwrap_or(false);
                            if owns_sink {
                                // 先恢复终端输出，再回执：UI 收到结果即可继续输入。
                                *sink
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                                if result.is_err() {
                                    // 远端可能停在 rz 的交互里：发 Ctrl+C 回到 shell。
                                    let _ = raw.send(vec![0x03]);
                                }
                            }
                            // 协议结束后 shell 的提示符可能已经排进接管通道：
                            // 恢复终端后补投，避免用户以为会话卡住。
                            while let Ok(bytes) = down_rx.try_recv() {
                                feed.feed(bytes);
                            }
                            let _ = reply.send(result);
                        });
                    }
                    SessionCommand::ZmodemCancel { reply } => {
                        // 先置位取消标记（send_file 每轮循环检查），再摘接管点
                        // 恢复终端输出，最后发 Ctrl+C 让远端 rz 退出回 shell。
                        // 顺序保证 send_file 收场时接管点已不在，不会重复发 ^C。
                        if let Some(slot) = zmodem_sink
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .as_ref()
                        {
                            slot.cancel();
                        }
                        *zmodem_sink
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                        let _ = raw_tx.send(vec![0x03]);
                        let _ = reply.send(());
                    }
                    SessionCommand::ForwardStart { rule, reply } => {
                        let result = match rule.kind {
                            ForwardKind::Local => start_local_forward(&accepted_tx, rule.clone())
                                .await
                                .map(|active| {
                                    local_forwards.insert(rule.listen_port, active);
                                    rule
                                }),
                            ForwardKind::Remote => start_remote_forward(&handle, rule)
                                .await
                                .map(|(effective, active)| {
                                    remote_forwards.insert(effective.listen_port, active);
                                    effective
                                }),
                            ForwardKind::Dynamic => {
                                start_dynamic_forward(&accepted_tx, rule.clone())
                                    .await
                                    .map(|active| {
                                        dynamic_forwards.insert(rule.listen_port, active);
                                        rule
                                    })
                            }
                        };
                        let _ = reply.send(result);
                    }
                    SessionCommand::ForwardStop { rule, reply } => {
                        let stopped = match rule.kind {
                            ForwardKind::Local => {
                                let mut forward = local_forwards.remove(&rule.listen_port);
                                stop_forward_task(&mut forward).await
                            }
                            ForwardKind::Remote => {
                                let mut forward = remote_forwards.remove(&rule.listen_port);
                                if forward.is_some()
                                    && let Err(error) = handle
                                        .cancel_tcpip_forward(REMOTE_BIND, u32::from(rule.listen_port))
                                        .await
                                {
                                    log::warn!("取消远程转发 {} 失败: {error}", rule.label());
                                }
                                stop_forward_task(&mut forward).await
                            }
                            ForwardKind::Dynamic => {
                                let mut forward = dynamic_forwards.remove(&rule.listen_port);
                                stop_forward_task(&mut forward).await
                            }
                        };
                        let _ = reply.send(stopped);
                    }
                }
            }
            Some(accepted) = accepted_rx.recv() => match accepted {
                AcceptedForward::Direct { rule, mut socket, peer_port } => {
                    // 停止转发的瞬间可能还有连接躺在队列里：此时按规则找不到归属，直接丢弃。
                    let Some(forward) = local_forwards.get_mut(&rule.listen_port) else {
                        continue;
                    };
                    match open_direct_tcpip(
                        &handle,
                        &side,
                        &rule.target_host,
                        rule.target_port,
                        peer_port,
                        &rule.label(),
                    )
                    .await
                    {
                        Ok(channel) => {
                            let mut stream = channel.into_stream();
                            forward.pumps.spawn(async move {
                                if let Err(error) =
                                    tokio::io::copy_bidirectional(&mut socket, &mut stream).await
                                {
                                    log::debug!("端口转发连接结束：{error}");
                                }
                            });
                        }
                        Err(error) => {
                            log::warn!("端口转发 {} 打开远端通道失败: {error}", rule.label());
                        }
                    }
                }
                AcceptedForward::Socks {
                    rule,
                    mut socket,
                    target_host,
                    target_port,
                    peer_port,
                } => {
                    let Some(forward) = dynamic_forwards.get_mut(&rule.listen_port) else {
                        continue;
                    };
                    let label = format!("D {} → {target_host}:{target_port}", rule.listen_port);
                    match open_direct_tcpip(
                        &handle,
                        &side,
                        &target_host,
                        target_port,
                        peer_port,
                        &label,
                    )
                    .await
                    {
                        Ok(channel) => {
                            let mut stream = channel.into_stream();
                            forward.pumps.spawn(async move {
                                // SOCKS5 成功应答必须在通道建立后才发，客户端据此开始发送数据。
                                if !socks_reply(&mut socket, SOCKS_REP_OK).await {
                                    return;
                                }
                                if let Err(error) =
                                    tokio::io::copy_bidirectional(&mut socket, &mut stream).await
                                {
                                    log::debug!("动态转发连接结束：{error}");
                                }
                            });
                        }
                        Err(error) => {
                            log::warn!("动态转发 {label} 打开远端通道失败: {error}");
                            // 连接失败也要回 SOCKS5 应答，否则客户端只能干等超时。
                            socks_reply(&mut socket, SOCKS_REP_REFUSED).await;
                        }
                    }
                }
            },
            Some(forwarded) = forwarded_rx.recv() => {
                // 服务器每接受一个外部连接就送一条 forwarded-tcpip 通道；
                // 找不到对应规则（已停止）就丢弃，通道随之关闭。
                let port = u16::try_from(forwarded.connected_port).unwrap_or(0);
                let Some(forward) = remote_forwards.get_mut(&port) else {
                    log::warn!(
                        "远程转发收到未登记的连接 {}:{}（可能规则已停止）",
                        forwarded.connected_address,
                        forwarded.connected_port
                    );
                    continue;
                };
                let target_host = forward.rule.target_host.clone();
                let target_port = forward.rule.target_port;
                let mut stream = forwarded.channel.into_stream();
                log::debug!(
                    "远程转发 {}:{} ← {}:{}，拨本机 {target_host}:{target_port}",
                    forwarded.connected_address,
                    forwarded.connected_port,
                    forwarded.originator_address,
                    forwarded.originator_port,
                );
                forward.pumps.spawn(async move {
                    const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
                    let dial = TcpStream::connect((target_host.as_str(), target_port));
                    let mut socket = match tokio::time::timeout(DIAL_TIMEOUT, dial).await {
                        Ok(Ok(socket)) => socket,
                        Ok(Err(error)) => {
                            log::warn!("远程转发无法连接本机 {target_host}:{target_port}: {error}");
                            return;
                        }
                        Err(_) => {
                            log::warn!("远程转发连接本机 {target_host}:{target_port} 超时");
                            return;
                        }
                    };
                    if let Err(error) =
                        tokio::io::copy_bidirectional(&mut socket, &mut stream).await
                    {
                        log::debug!("远程转发连接结束：{error}");
                    }
                });
            }
            _ = health.tick() => {
                // 结束的泵如果留在 JoinSet 里会一直占内存，定期回收。
                for forward in local_forwards.values_mut() {
                    while forward.pumps.try_join_next().is_some() {}
                }
                for forward in remote_forwards.values_mut() {
                    while forward.pumps.try_join_next().is_some() {}
                }
                for forward in dynamic_forwards.values_mut() {
                    while forward.pumps.try_join_next().is_some() {}
                }
                if handle.is_closed() {
                    anyhow::bail!("SSH 连接已断开");
                }
            }
        }
    }
    for (_, mut forward) in local_forwards {
        forward.pumps.shutdown().await;
    }
    for (_, mut forward) in remote_forwards {
        forward.pumps.shutdown().await;
    }
    for (_, mut forward) in dynamic_forwards {
        forward.pumps.shutdown().await;
    }
    Ok(())
}

/// 停止一条转发：收掉监听与该规则的全部活动连接。
async fn stop_forward_task(task: &mut Option<ActiveForward>) -> bool {
    let Some(forward) = task else {
        return false;
    };
    forward.pumps.shutdown().await;
    true
}

/// 开始一条本地端口转发：监听本机回环地址，accept 到的连接交给会话循环。
///
/// 只绑 `127.0.0.1`，不把端口暴露到局域网，避免在用户不知情时成为开放代理。
/// 返回的监听任务由调用方登记，停止/会话结束时统一 abort，端口随之释放。
async fn start_local_forward(
    accepted: &mpsc::UnboundedSender<AcceptedForward>,
    rule: ForwardRule,
) -> Result<ActiveForward> {
    let listener = TcpListener::bind(("127.0.0.1", rule.listen_port))
        .await
        .with_context(|| format!("无法监听本地端口 {}（可能已被占用）", rule.listen_port))?;
    let mut pumps = JoinSet::new();
    pumps.spawn(forward_accept_loop(
        listener,
        accepted.clone(),
        rule.clone(),
    ));
    Ok(ActiveForward { rule, pumps })
}

/// 开始一条动态转发（-D）：本机回环上的 SOCKS5 代理。
///
/// 目标和 `-L` 一样只绑 `127.0.0.1`，不会成为局域网可用的开放代理。
async fn start_dynamic_forward(
    accepted: &mpsc::UnboundedSender<AcceptedForward>,
    rule: ForwardRule,
) -> Result<ActiveForward> {
    let listener = TcpListener::bind(("127.0.0.1", rule.listen_port))
        .await
        .with_context(|| format!("无法监听 SOCKS5 端口 {}（可能已被占用）", rule.listen_port))?;
    let mut pumps = JoinSet::new();
    pumps.spawn(dynamic_accept_loop(
        listener,
        accepted.clone(),
        rule.clone(),
    ));
    Ok(ActiveForward { rule, pumps })
}

/// 远程转发只绑服务器回环：不把服务器端口暴露到公网，避免变成开放代理。
const REMOTE_BIND: &str = "127.0.0.1";

/// 请求服务器监听一个端口（-R）。
///
/// 服务器可能分配与请求不同的端口（请求 0 时由服务器挑选），返回生效规则。
async fn start_remote_forward(
    handle: &Handle<SshTerminalHandler>,
    mut rule: ForwardRule,
) -> Result<(ForwardRule, ActiveForward)> {
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
    let requested = rule.listen_port;
    let assigned = tokio::time::timeout(
        REQUEST_TIMEOUT,
        handle.tcpip_forward(REMOTE_BIND, u32::from(requested)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("请求远程转发超时（服务器未响应）"))?
    .context("服务器拒绝远程转发（检查 sshd 的 AllowTcpForwarding / GatewayPorts）")?;
    // 部分实现请求具体端口时回 0：此时以请求值为准。
    let assigned = u16::try_from(assigned).unwrap_or(0);
    let assigned = if assigned == 0 { requested } else { assigned };
    if assigned == 0 {
        anyhow::bail!("服务器未返回可用的监听端口");
    }
    rule.listen_port = assigned;
    Ok((
        rule.clone(),
        ActiveForward {
            rule,
            pumps: JoinSet::new(),
        },
    ))
}

/// 接受本地连接，把套接字交给会话循环去开 direct-tcpip 通道。
///
/// 会话结束时接收端被丢弃，send 失败即退出，监听随之释放。
async fn forward_accept_loop(
    listener: TcpListener,
    accepted: mpsc::UnboundedSender<AcceptedForward>,
    rule: ForwardRule,
) {
    loop {
        let Ok((socket, peer)) = listener.accept().await else {
            log::debug!("端口转发监听结束：{}", rule.label());
            return;
        };
        if accepted
            .send(AcceptedForward::Direct {
                rule: rule.clone(),
                socket,
                peer_port: peer.port(),
            })
            .is_err()
        {
            log::debug!("端口转发会话已结束：{}", rule.label());
            return;
        }
    }
}

/// 动态转发的 accept 循环：每条连接先在自己的任务里做 SOCKS5 握手，
/// 拿到目标再交给会话循环开通道，慢客户端不会卡住 accept。
async fn dynamic_accept_loop(
    listener: TcpListener,
    accepted: mpsc::UnboundedSender<AcceptedForward>,
    rule: ForwardRule,
) {
    loop {
        let Ok((mut socket, peer)) = listener.accept().await else {
            log::debug!("SOCKS5 监听结束：{}", rule.label());
            return;
        };
        let accepted = accepted.clone();
        let rule = rule.clone();
        tokio::spawn(async move {
            let (target_host, target_port) = match socks_handshake(&mut socket).await {
                Ok(target) => target,
                Err(error) => {
                    log::debug!("SOCKS5 握手失败（{}）: {error}", peer);
                    return;
                }
            };
            if accepted
                .send(AcceptedForward::Socks {
                    rule,
                    socket,
                    target_host,
                    target_port,
                    peer_port: peer.port(),
                })
                .is_err()
            {
                log::debug!("SOCKS5 会话已结束，丢弃连接 {peer}");
            }
        });
    }
}

/// SOCKS5 应答：成功 / 连接被拒，BND.ADDR 用 0.0.0.0:0（客户端不关心）。
const SOCKS_REP_OK: u8 = 0x00;
const SOCKS_REP_REFUSED: u8 = 0x05;
const SOCKS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 回一条 SOCKS5 应答；返回是否发送成功（回环写一般不会阻塞太久）。
async fn socks_reply(socket: &mut TcpStream, rep: u8) -> bool {
    const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
    let reply = [0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    matches!(
        tokio::time::timeout(REPLY_TIMEOUT, socket.write_all(&reply)).await,
        Ok(Ok(()))
    )
}

/// 带超时的 `read_exact`：客户端连上却不说话时不让任务一直挂着。
async fn read_exact_timeout(socket: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    tokio::time::timeout(SOCKS_HANDSHAKE_TIMEOUT, socket.read_exact(buf))
        .await
        .map_err(|_| anyhow::anyhow!("SOCKS5 握手超时"))?
        .map(|_| ())
        .context("SOCKS5 握手连接中断")
}

/// 完成 SOCKS5 握手（无认证 + CONNECT），返回客户端请求的目标地址。
///
/// 只支持 CONNECT；协商失败或目标不支持时尽量回一条 RFC 1928 应答再断开，
/// 让 curl / 浏览器立刻报错而不是等到超时。
async fn socks_handshake(socket: &mut TcpStream) -> Result<(String, u16)> {
    let mut head = [0u8; 2];
    read_exact_timeout(socket, &mut head).await?;
    if head[0] != 0x05 {
        anyhow::bail!("不是 SOCKS5 请求（版本 {}）", head[0]);
    }
    let methods = head[1] as usize;
    if methods == 0 {
        anyhow::bail!("客户端没有提供认证方式");
    }
    let mut offered = vec![0u8; methods];
    read_exact_timeout(socket, &mut offered).await?;
    if !offered.contains(&0x00) {
        socks_reply_raw(socket, 0x05, 0xFF).await;
        anyhow::bail!("客户端要求认证，动态转发只支持无认证");
    }
    if !socks_reply_raw(socket, 0x05, 0x00).await {
        anyhow::bail!("回写 SOCKS5 握手应答失败");
    }

    let mut request = [0u8; 4];
    read_exact_timeout(socket, &mut request).await?;
    if request[0] != 0x05 || request[2] != 0x00 {
        anyhow::bail!("SOCKS5 请求格式错误");
    }
    let target_host = match request[3] {
        0x01 => {
            let mut bytes = [0u8; 4];
            read_exact_timeout(socket, &mut bytes).await?;
            Ipv4Addr::from(bytes).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            read_exact_timeout(socket, &mut len).await?;
            if len[0] == 0 {
                socks_reply(socket, 0x08).await;
                anyhow::bail!("SOCKS5 域名为空");
            }
            let mut name = vec![0u8; len[0] as usize];
            read_exact_timeout(socket, &mut name).await?;
            let name = String::from_utf8(name).context("SOCKS5 域名不是合法 UTF-8")?;
            // 域名会原样交给 sshd 解析，去掉首尾空白避免「 example.com」这种无效目标。
            let name = name.trim().to_string();
            if name.is_empty() {
                socks_reply(socket, 0x08).await;
                anyhow::bail!("SOCKS5 域名为空");
            }
            name
        }
        0x04 => {
            let mut bytes = [0u8; 16];
            read_exact_timeout(socket, &mut bytes).await?;
            Ipv6Addr::from(bytes).to_string()
        }
        other => {
            socks_reply(socket, 0x08).await; // Address type not supported
            anyhow::bail!("不支持的地址类型 {other:#04x}");
        }
    };
    let mut port = [0u8; 2];
    read_exact_timeout(socket, &mut port).await?;
    let target_port = u16::from_be_bytes(port);
    // 先把整条请求读完再拒绝：还有未读数据时直接关 socket 会发 RST，
    // 客户端可能收不到那条拒绝应答。
    if request[1] != 0x01 {
        socks_reply(socket, 0x07).await; // Command not supported
        anyhow::bail!("只支持 CONNECT（收到命令 {:#04x}）", request[1]);
    }
    if target_port == 0 {
        socks_reply(socket, SOCKS_REP_REFUSED).await;
        anyhow::bail!("SOCKS5 目标端口为 0");
    }
    Ok((target_host, target_port))
}

/// 回写握手阶段的两字节应答（版本 + 方法）。
async fn socks_reply_raw(socket: &mut TcpStream, version: u8, method: u8) -> bool {
    matches!(
        tokio::time::timeout(
            SOCKS_HANDSHAKE_TIMEOUT,
            socket.write_all(&[version, method])
        )
        .await,
        Ok(Ok(()))
    )
}

/// 为一条连接打开 direct-tcpip 通道，并登记进旁路集合。
async fn open_direct_tcpip(
    handle: &Handle<SshTerminalHandler>,
    side: &SideChannels,
    target_host: &str,
    target_port: u16,
    peer_port: u16,
    label: &str,
) -> Result<Channel<Msg>> {
    const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
    let open = handle.channel_open_direct_tcpip(
        target_host.to_string(),
        u32::from(target_port),
        "127.0.0.1",
        u32::from(peer_port),
    );
    let channel = tokio::time::timeout(OPEN_TIMEOUT, open)
        .await
        .map_err(|_| anyhow::anyhow!("打开远端通道超时（{label}）"))?
        .with_context(|| format!("远端拒绝转发到 {target_host}:{target_port}"))?;
    // direct-tcpip 也是旁路通道：不登记的话，它的字节会灌进终端、
    // close/EOF 还会被误判成"会话结束"。
    side.add(channel.id());
    Ok(channel)
}

/// 在现有连接上开一条 SFTP 子系统通道。
///
/// 通道登记进旁路集合，避免 SFTP 的字节与控制消息被当成终端输出。
/// 服务器没启用 SFTP 时不能让会话循环永远卡在这里，整体限时。
async fn open_sftp(
    handle: &Handle<SshTerminalHandler>,
    side: &SideChannels,
) -> Result<SftpSession> {
    const SFTP_TIMEOUT: Duration = Duration::from_secs(10);
    let open = async {
        let channel = handle
            .channel_open_session()
            .await
            .context("无法开启 SFTP 通道")?;
        side.add(channel.id());
        channel
            .request_subsystem(true, "sftp")
            .await
            .context("服务器未启用 SFTP 子系统")?;
        SftpSession::new_with_config(channel.into_stream(), transfer::client_config())
            .await
            .context("SFTP 会话初始化失败")
    };
    tokio::time::timeout(SFTP_TIMEOUT, open)
        .await
        .map_err(|_| anyhow::anyhow!("SFTP 通道超时（服务器未响应）"))?
}

/// 取文件面板的 SFTP 会话；首次访问时打开并缓存。
async fn cached_sftp(
    handle: &Handle<SshTerminalHandler>,
    side: &SideChannels,
    cache: &Mutex<Option<Arc<SftpSession>>>,
) -> Result<Arc<SftpSession>> {
    {
        let guard = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sftp) = guard.as_ref() {
            return Ok(sftp.clone());
        }
    }
    // 打开期间不持锁（内含 await）；并发第二条命令会各自开通道，谁先放进
    // 缓存谁赢，输家用完随任务回收（多余的通道由 Drop 关闭）。
    let sftp = Arc::new(open_sftp(handle, side).await?);
    let mut guard = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = Some(sftp.clone());
    }
    drop(guard);
    Ok(sftp)
}

/// 丢弃文件面板的 SFTP 缓存：通道出错后下次点击重开子系统，
/// 避免抱着坏通道反复报错。循环内与 spawn 出去的任务都会用到。
fn drop_cached_sftp(cache: &Mutex<Option<Arc<SftpSession>>>) {
    *cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// 另开一条 exec 通道采集远程主机基本信息，连接期间周期回传状态栏需要刷新的指标。
///
/// 单条通道里循环执行探测脚本（每次输出以 `end` 行收尾），避免每隔几秒新建通道；
/// 探测失败（远端禁用 exec、通道打不开、首次输出超时）只记日志，不影响交互会话。
async fn probe_remote_info(
    handle: &Handle<SshTerminalHandler>,
    side: SideChannels,
    info_tx: mpsc::UnboundedSender<RemoteInfo>,
) {
    let Ok(probe) = handle.channel_open_session().await else {
        log::debug!("无法开启远程信息探测通道");
        return;
    };
    side.add(probe.id());
    let (mut read, write) = probe.split();
    let command = format!(
        "while :; do {PROBE_CMD}; printf 'end\\n'; sleep {}; done",
        PROBE_INTERVAL.as_secs()
    );
    if write.exec(false, command.into_bytes()).await.is_err() {
        log::debug!("远程信息探测命令下发失败");
        return;
    }

    tokio::spawn(async move {
        let mut pending: Vec<u8> = Vec::new();
        let mut block: Vec<u8> = Vec::new();
        let mut first = true;
        loop {
            // 只给首块限时：连上后远端明明能跑脚本却一直没输出，才当作探测不可用。
            let msg = if first {
                match tokio::time::timeout(PROBE_TIMEOUT, read.wait()).await {
                    Ok(msg) => msg,
                    Err(_) => {
                        log::debug!("远程信息探测超时，跳过状态栏展示");
                        break;
                    }
                }
            } else {
                read.wait().await
            };
            let Some(msg) = msg else { break };
            match msg {
                ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                    pending.extend_from_slice(&data);
                    let mut start = 0;
                    while let Some(offset) = pending[start..].iter().position(|&b| b == b'\n') {
                        let line = &pending[start..start + offset];
                        start += offset + 1;
                        if String::from_utf8_lossy(line).trim() == "end" {
                            if !block.is_empty() {
                                let info = RemoteInfo::parse(&String::from_utf8_lossy(&block));
                                block.clear();
                                if info.is_empty() || info_tx.send(info).is_err() {
                                    // UI 已不再需要（标签关闭）或远端什么都没给：停止轮询。
                                    let _ = write.close().await;
                                    return;
                                }
                                first = false;
                            }
                        } else {
                            block.extend_from_slice(line);
                            block.push(b'\n');
                        }
                    }
                    pending.drain(..start);
                }
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = write.close().await;
    });
}

/// 让远端 shell 自己上报工作目录（OSC 7 `file://host/$PWD`），文件面板的
/// 「跟随终端 / 定位到终端目录」才有确定来源：靠解析按键里的 `cd` 只能猜，
/// 带 Tab 补全、方向键调历史、变量替换的行一律认不出来。
///
/// 先另开一条 exec 通道读 `$SHELL`（sshd 按 passwd 起交互 shell，这个变量对
/// csh / fish 也照样能 echo 出来），确认是 bash / zsh 才注入钩子；其余 shell
/// 语法不同（fish 无 `()` 函数定义、csh 无 PROMPT_COMMAND），注入会刷一屏报错，
/// 就退回按键解析。
async fn install_cwd_report(
    handle: &Handle<SshTerminalHandler>,
    side: &SideChannels,
    raw_tx: &mpsc::UnboundedSender<Vec<u8>>,
    last_input: &Arc<Mutex<Option<Instant>>>,
) {
    let Some(shell) = login_shell(handle, side).await else {
        log::debug!("[shell_integration] 探测登录 shell 失败，沿用按键解析");
        return;
    };
    if !reports_cwd(&shell) {
        log::debug!("[shell_integration] {shell} 不支持 OSC 7 注入，沿用按键解析");
        return;
    }
    let tx = raw_tx.clone();
    let last_input = last_input.clone();
    // 提示符（含 MOTD）落地后再写这行：太早发会接在用户已经敲了半行的命令后面。
    // 固定延时只对"连上就干等"的用户成立；用户已经开始敲键时推迟到输入静默
    // 一段时间再写，否则注入行会拼进半行命令、或写进已经进入的 vim / 嵌套
    // 会话里。宁可一直等（面板的 cd 按键解析兜底不受影响），也不污染输入。
    tokio::spawn(async move {
        tokio::time::sleep(SHELL_INTEGRATION_DELAY).await;
        loop {
            let quiet = last_input
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .map(|at| at.elapsed() >= SHELL_INTEGRATION_QUIET)
                .unwrap_or(true);
            if quiet {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let _ = tx.send(format!("{SHELL_INTEGRATION}\r").into_bytes());
    });
}

/// 探测登录 shell 的路径：exec 通道跑 `echo $SHELL`，取输出的第一行。
///
/// 通道登记进 [`SideChannels`]，探测输出不会灌进终端；服务器分段回数据，所以要
/// 读到换行（或通道收尾）为止，只拿第一条 DATA 会把 `/bin/bash` 截成 `/bin/ba`。
/// 探测不可用（禁 exec、超时、输出不是绝对路径）统一返回 None。
async fn login_shell(handle: &Handle<SshTerminalHandler>, side: &SideChannels) -> Option<String> {
    let probe = handle.channel_open_session().await.ok()?;
    side.add(probe.id());
    let (mut read, write) = probe.split();
    if write.exec(false, b"echo $SHELL".to_vec()).await.is_err() {
        return None;
    }
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    let mut output: Vec<u8> = Vec::new();
    while !output.contains(&b'\n') && output.len() < 4096 {
        let Ok(Some(msg)) = tokio::time::timeout_at(deadline, read.wait()).await else {
            break;
        };
        match msg {
            ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                output.extend_from_slice(&data);
            }
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    let _ = write.close().await;
    shell_path_from_output(&output)
}

/// 探测输出的第一行：是绝对路径才认，Windows 上 `echo $SHELL` 回显字面量就是 None。
fn shell_path_from_output(output: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(output)
        .lines()
        .next()?
        .trim()
        .to_string();
    line.starts_with('/').then_some(line)
}

/// 只有 bash / zsh 会注入：登录 shell 名带前导 `-`（login shell 的写法）也要认。
fn reports_cwd(shell: &str) -> bool {
    let name = shell
        .rsplit('/')
        .next()
        .unwrap_or(shell)
        .trim_start_matches('-');
    matches!(name, "bash" | "zsh")
}

/// 注入等待提示符的时间。
const SHELL_INTEGRATION_DELAY: Duration = Duration::from_millis(1200);
/// 用户输入要静默多久才允许注入 OSC 7 钩子。
const SHELL_INTEGRATION_QUIET: Duration = Duration::from_secs(5);

/// 同一份代码兼顾 bash（追加 `PROMPT_COMMAND`）与 zsh（`precmd` 钩子），两者都在
/// 每次显示提示符前上报 `$PWD`；末尾那次调用立即报一次，连接上就能看到当前目录。
///
/// 开头的空格让 bash 在 `HISTCONTROL=ignorespace` 下不把这行记进历史；已有的
/// `PROMPT_COMMAND` 保留在后面，不覆盖用户自己的钩子。
const SHELL_INTEGRATION: &str = concat!(
    " __rc(){ printf '\\033]7;file://%s%s\\007' \"${HOSTNAME:-localhost}\" \"$PWD\"; }; ",
    "if [ -n \"$ZSH_VERSION\" ]; then autoload -U add-zsh-hook 2>/dev/null; ",
    "add-zsh-hook precmd __rc 2>/dev/null; ",
    "else PROMPT_COMMAND=\"__rc${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"; fi; __rc",
);

/// SSH 会话要声明的 locale 变量：本地显式设置的优先透传，否则默认 C.UTF-8。
fn ssh_locale_env() -> Vec<(&'static str, String)> {
    let mut env = Vec::new();
    for name in ["LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(name).filter(|v| !v.is_empty()) {
            env.push((name, value.to_string_lossy().into_owned()));
        }
    }
    if env.is_empty() {
        env.push(("LANG", "C.UTF-8".to_string()));
    }
    env
}

/// 终端 → 远端：按键字节写通道，尺寸变化发 window_change；
/// 另有原始字节通道给 lrzsz 上传直接写协议帧。
///
/// 同时逐字节扫描用户键入，识别 `cd` / `pushd` 行并作为目录线索回传，
/// 由 [`follow_cwd`] 校验后再回传 UI，避免 cd 失败或含变量替换时污染面板。
async fn forward_uplink(
    channel: SessionChannelWrite,
    mut ui_rx: mpsc::UnboundedReceiver<PtyBoundMessage>,
    mut raw_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cwd_hint_tx: mpsc::UnboundedSender<CwdHint>,
    last_input: Arc<Mutex<Option<Instant>>>,
) {
    let mut tracker = CwdKeystrokes::default();
    loop {
        let sent = tokio::select! {
            maybe = ui_rx.recv() => {
                let Some(message) = maybe else {
                    return;
                };
                match message {
                    PtyBoundMessage::Input(bytes) => {
                        // shell 集成注入的避让信号：用户动过键盘就先别注入。
                        *last_input
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(Instant::now());
                        // Key repeats arrive as hundreds of one-byte events. Writing each
                        // event separately can fill russh's bounded channel queue and leave
                        // the forwarding task waiting while the UI keeps enqueueing input.
                        // Coalesce only adjacent input messages; byte order is unchanged.
                        let mut bytes = bytes;
                        let mut resize_after_batch = None;
                        while bytes.len() < 64 * 1024 {
                            match ui_rx.try_recv() {
                                Ok(PtyBoundMessage::Input(next)) => {
                                    bytes.extend_from_slice(&next);
                                }
                                Ok(PtyBoundMessage::Resize(size)) => {
                                    resize_after_batch = Some(size);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        if let Some(arg) = tracker.feed(&bytes) {
                            let _ = cwd_hint_tx.send(CwdHint::Cd(arg));
                        }
                        let mut result = channel.data_bytes(Bytes::from(bytes)).await;
                        if result.is_ok()
                            && let Some(size) = resize_after_batch
                        {
                            result = channel
                                .window_change(
                                    u32::from(size.num_cols),
                                    u32::from(size.num_lines),
                                    u32::from(size.cell_width) * u32::from(size.num_cols),
                                    u32::from(size.cell_height) * u32::from(size.num_lines),
                                )
                                .await;
                        }
                        result
                    }
                    PtyBoundMessage::Resize(size) => {
                        channel
                            .window_change(
                                u32::from(size.num_cols),
                                u32::from(size.num_lines),
                                u32::from(size.cell_width) * u32::from(size.num_cols),
                                u32::from(size.cell_height) * u32::from(size.num_lines),
                            )
                            .await
                    }
                }
            }
            Some(bytes) = raw_rx.recv() => {
                channel.data_bytes(Bytes::from(bytes)).await
            }
            else => return,
        };
        if sent.is_err() {
            log::debug!("上行通道已失效，结束转发");
            return;
        }
    }
}

/// 逐字节跟踪用户键入：把终端的按键流还原成一行命令，识别 cd / pushd。
///
/// 遇到不可还原的编辑（方向键、Tab 补全、Ctrl+A 等）就把整行标成"不可信"，
/// 宁可漏报也不误报——面板跳错目录比不跟更糟。
#[derive(Default)]
struct CwdKeystrokes {
    buf: Vec<u8>,
    skip: bool,
}

impl CwdKeystrokes {
    fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        let mut done = None;
        for &b in bytes {
            match b {
                0x0d | 0x0a => {
                    if !self.skip
                        && let Ok(line) = std::str::from_utf8(&self.buf)
                        && let Some(arg) = cd_arg_from_line(line)
                    {
                        log::debug!("[CwdKeystrokes] 检测到 cd: {}", arg);
                        done = Some(arg);
                    } else if !self.skip
                        && let Ok(line) = std::str::from_utf8(&self.buf)
                    {
                        log::debug!("[CwdKeystrokes] 行结束但未检测到 cd: {:?}", line);
                    }
                    self.buf.clear();
                    self.skip = false;
                }
                0x03 | 0x15 => {
                    // Ctrl+C / Ctrl+U：清空缓冲区，整行作废。
                    self.buf.clear();
                    self.skip = false;
                }
                0x17 => {
                    // Ctrl+W：删除上一个词；只按 ASCII 空格切词，
                    // 遇到 UTF-8 多字节字符会留下残缺字节，finalize 时 from_utf8 失败 → 整行跳过。
                    while self.buf.last().is_some_and(|&b| b == b' ') {
                        self.buf.pop();
                    }
                    while self.buf.last().is_some_and(|&b| b != b' ') {
                        self.buf.pop();
                    }
                }
                0x7f | 0x08 => {
                    self.buf.pop();
                }
                0x1b | 0x09 => {
                    // ESC（方向键 / 历史 / 粘贴括号）或 Tab（补全）：行内容已不可还原。
                    self.skip = true;
                }
                b if b < 0x20 => {
                    // 其他控制字符（Ctrl+A/E/B/F 等）：行编辑已偏离缓冲区。
                    self.skip = true;
                }
                _ => {
                    if self.buf.len() < 4096 {
                        self.buf.push(b);
                    } else {
                        self.skip = true;
                    }
                }
            }
            if done.is_some() {
                break;
            }
        }
        done
    }
}

/// 从一行命令里提取 cd / pushd 的目标路径（空串＝回主目录）。
///
/// 仅处理最简情形：行首命令是 `cd` 或 `pushd`，参数在第一个分号/管道/逻辑与/注释前，
/// 成对引号会被剥掉；其余情形（含变量替换、通配、~user、pushd 无参等）一律返回 None，
/// 由校验任务保守地跳过，避免面板跳到错误目录。
fn cd_arg_from_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (cmd, rest) = match line.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (line, ""),
    };
    if cmd != "cd" && cmd != "pushd" {
        return None;
    }
    // pushd 无参 = 交换栈顶，无法跟踪；cd 无参 = 回主目录。
    if cmd == "pushd" && rest.is_empty() {
        return None;
    }
    let first = rest.split([';', '|', '&', '#']).next()?.trim();
    let mut arg = first;
    if arg.len() >= 2
        && let Some(stripped) = strip_matching_quotes(arg)
    {
        arg = stripped;
    }
    // 丢弃前导选项（-L / -P / -e ...），但保留裸 "-"（回 OLDPWD）。
    while arg.starts_with('-') && arg != "-" && arg.len() > 1 {
        match arg.split_once(char::is_whitespace) {
            Some((_, tail)) => arg = tail.trim(),
            None => return None,
        }
    }
    if arg.contains('$')
        || arg.contains('`')
        || arg.contains('*')
        || arg.contains('?')
        || (arg.starts_with('~') && arg != "~" && !arg.starts_with("~/"))
    {
        return None;
    }
    Some(arg.to_string())
}

fn strip_matching_quotes(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let first = bytes.first()?;
    let last = bytes.last()?;
    if (first == &b'"' && last == &b'"') || (first == &b'\'' && last == &b'\'') {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

/// 把相对路径按 POSIX 规则词法拼接到 base（base 必须绝对），
/// 不查文件系统：`..` 弹栈、`.` 跳过、空段忽略。cd 失败时校验任务会拒收，
/// 所以这里的"乐观拼接"不会污染面板。
fn lex_join(base: &str, rel: &str) -> String {
    let mut stack: Vec<&str> = base
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        let mut out = String::with_capacity(stack.iter().map(|s| s.len() + 1).sum());
        out.push('/');
        for (i, seg) in stack.iter().enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(seg);
        }
        out
    }
}

/// 百分号解码（`%XX` → 字节），非 UTF-8 序列用 lossy 还原；
/// 无效 `%` 序列原样保留，避免把用户路径里的 `%` 误吞。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];
            if let (Some(h), Some(l)) = (hex_digit(hi), hex_digit(lo)) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 解析 OSC 7 上报的 `file://host/path` 或 `file:///path`，返回解码后的绝对路径。
pub fn path_from_osc7(payload: &str) -> Option<String> {
    let rest = payload.strip_prefix("file://")?;
    let idx = rest.find('/')?;
    let path = &rest[idx..];
    if !path.starts_with('/') {
        return None;
    }
    Some(percent_decode(path))
}

/// 校验任务：按序处理 cd / OSC 7 线索，解析出绝对路径后用 SFTP 确认是目录，
/// 成功才回传 UI。顺序处理避免快速连 cd 时校验乱序导致面板跳错。
async fn follow_cwd(
    mut hints: mpsc::UnboundedReceiver<CwdHint>,
    commands: mpsc::UnboundedSender<SessionCommand>,
    confirmed: mpsc::UnboundedSender<String>,
) {
    let mut cwd: Option<String> = None;
    let mut old: Option<String> = None;
    let mut home: Option<String> = None;

    while let Some(hint) = hints.recv().await {
        log::debug!("[follow_cwd] 收到线索: {:?}", hint);
        let (candidate, is_cd) = match hint {
            CwdHint::Report(path) => {
                if !path.starts_with('/') {
                    continue;
                }
                (Some(path), false)
            }
            CwdHint::Cd(arg) => {
                let arg = arg.trim().to_string();
                let resolved = if arg.is_empty() || arg == "~" {
                    fetch_home(&commands, &mut home).await
                } else if arg == "-" {
                    old.clone()
                } else if let Some(rest) = arg.strip_prefix("~/") {
                    fetch_home(&commands, &mut home)
                        .await
                        .map(|h| lex_join(&h, rest))
                } else if arg.starts_with('/') {
                    Some(arg)
                } else {
                    // 相对路径：以当前 cwd 为基准；首次 cd 时 shell 默认在主目录，
                    // 同步取一次 home 作为起点，避免首条相对 cd 被丢弃。
                    let base = if let Some(b) = cwd.clone() {
                        Some(b)
                    } else {
                        fetch_home(&commands, &mut home).await
                    };
                    base.map(|b| lex_join(&b, &arg))
                };
                (resolved, true)
            }
        };
        let Some(path) = candidate else {
            log::debug!("[follow_cwd] 线索无法解析，跳过");
            continue;
        };
        log::debug!("[follow_cwd] 校验路径: {}", path);
        let (reply, rx) = oneshot::channel();
        if commands
            .send(SessionCommand::FsStat {
                path: path.clone(),
                reply,
            })
            .is_err()
        {
            return;
        }
        match rx.await {
            Ok(Ok(true)) => {
                log::debug!("[follow_cwd] 校验通过，发送路径: {}", path);
                if is_cd {
                    old = cwd.clone();
                    cwd = Some(path.clone());
                } else {
                    cwd = Some(path.clone());
                }
                let _ = confirmed.send(path);
            }
            Ok(Ok(false)) => {
                log::debug!("[follow_cwd] 校验失败（非目录）: {}", path);
                if is_cd {
                    cwd = old.clone();
                }
            }
            Ok(Err(e)) => {
                log::debug!("[follow_cwd] 校验出错: {}，路径: {}", e, path);
                if is_cd {
                    cwd = old.clone();
                }
            }
            Err(_) => {
                log::debug!("[follow_cwd] 校验通道关闭");
                if is_cd {
                    cwd = old.clone();
                }
            }
        }
    }
}

async fn fetch_home(
    commands: &mpsc::UnboundedSender<SessionCommand>,
    home: &mut Option<String>,
) -> Option<String> {
    if let Some(h) = home.clone() {
        return Some(h);
    }
    let (reply, rx) = oneshot::channel();
    if commands.send(SessionCommand::Home { reply }).is_err() {
        return None;
    }
    match rx.await {
        Ok(Ok(h)) => {
            *home = Some(h.clone());
            Some(h)
        }
        _ => None,
    }
}

/// 按 密码 → 内嵌私钥 → 私钥文件 → SSH Agent 的优先级完成认证。
async fn authenticate(handle: &mut Handle<SshTerminalHandler>, target: &SshTarget) -> Result<()> {
    if let Some(password) = &target.password {
        let ok = handle
            .authenticate_password(target.user.clone(), password.clone())
            .await?
            .success();
        if !ok {
            return Err(anyhow::anyhow!("密码认证失败").context(SecretRejected));
        }
        return Ok(());
    }

    if let Some(data) = &target.key_data {
        let secret = match russh::keys::decode_secret_key(data, target.passphrase.as_deref()) {
            Ok(secret) => secret,
            Err(russh::keys::Error::KeyIsEncrypted) if target.passphrase.is_none() => {
                return Err(PassphraseRequired.into());
            }
            Err(error) => {
                let context = if target.passphrase.is_some() {
                    "内嵌私钥口令解密失败（口令可能不正确）"
                } else {
                    "内嵌私钥无法解析（内容可能已损坏）"
                };
                let mut error = anyhow::Error::new(error).context(context);
                if target.passphrase.is_some() {
                    error = error.context(SecretRejected);
                }
                return Err(error);
            }
        };
        return authenticate_key(handle, target, secret, "内嵌私钥").await;
    }

    if let Some(path) = &target.key_path {
        let secret = match russh::keys::load_secret_key(path, target.passphrase.as_deref()) {
            Ok(secret) => secret,
            // 密钥有口令但本次没带：以专用错误上抛，UI 弹口令窗而不是连接失败一闪而过。
            Err(russh::keys::Error::KeyIsEncrypted) if target.passphrase.is_none() => {
                return Err(PassphraseRequired.into());
            }
            Err(error) => {
                let context = if target.passphrase.is_some() {
                    format!("私钥口令解密失败 {}（口令可能不正确）", path.display())
                } else {
                    format!("无法加载私钥 {}", path.display())
                };
                let mut error = anyhow::Error::new(error).context(context);
                // 带了口令还解不开＝口令失效，按"秘密被拒"处理（清缓存/库内条目）。
                if target.passphrase.is_some() {
                    error = error.context(SecretRejected);
                }
                return Err(error);
            }
        };
        return authenticate_key(handle, target, secret, &path.display().to_string()).await;
    }

    // SSH Agent：逐个尝试 agent 中登记的密钥（Unix 走 SSH_AUTH_SOCK，
    // Windows 经同名环境变量取命名管道路径）。
    #[cfg(unix)]
    let mut agent = AgentClient::connect_env()
        .await
        .context("未提供密码/私钥，且无法连接 SSH Agent")?;
    #[cfg(windows)]
    let mut agent = {
        // OpenSSH for Windows 的 agent 服务监听固定命名管道，且不设
        // SSH_AUTH_SOCK（那是 Unix 的约定）——没设就回退默认管道，让
        // Windows 上选 Agent 认证的用户开箱可用，而不是必须手动 setx。
        let pipe = std::env::var_os("SSH_AUTH_SOCK")
            .unwrap_or_else(|| std::ffi::OsString::from(r"\\.\pipe\openssh-ssh-agent"));
        AgentClient::connect_named_pipe(pipe)
            .await
            .context("无法连接 SSH Agent（默认管道连不上：openssh-ssh-agent 服务可能未运行）")?
    };
    let identities = agent.request_identities().await?;
    anyhow::ensure!(!identities.is_empty(), "SSH Agent 中没有可用密钥");
    for identity in &identities {
        let public_key = identity.public_key().as_ref().clone();
        if handle
            .authenticate_publickey_with(target.user.clone(), public_key, None, &mut agent)
            .await?
            .success()
        {
            return Ok(());
        }
    }
    Err(anyhow::anyhow!("SSH Agent 中的密钥均被服务器拒绝").context(SecretRejected))
}

/// 用已加载的私钥完成公钥认证（文件私钥与内嵌私钥共用）。
async fn authenticate_key(
    handle: &mut Handle<SshTerminalHandler>,
    target: &SshTarget,
    secret: russh::keys::PrivateKey,
    label: &str,
) -> Result<()> {
    let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
    let key = PrivateKeyWithHashAlg::new(Arc::new(secret), hash_alg);
    let ok = handle
        .authenticate_publickey(target.user.clone(), key)
        .await?
        .success();
    if !ok {
        return Err(anyhow::anyhow!(
            "公钥认证被服务器拒绝：{label}（请确认此私钥与服务器上授权的公钥匹配；若服务器只允许密码，请把该主机的认证方式改为密码）"
        )
        .context(SecretRejected));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ForwardKind, ForwardRule};
    use anyhow::Result;
    use tokio::net::{TcpListener, TcpStream};

    fn rule(kind: ForwardKind, listen_port: u16) -> ForwardRule {
        ForwardRule {
            kind,
            listen_port,
            target_host: "127.0.0.1".to_string(),
            target_port: 80,
        }
    }

    #[test]
    fn forward_rule_labels_distinguish_direction() {
        assert_eq!(
            rule(ForwardKind::Local, 8080).label(),
            "L 127.0.0.1:8080 → 127.0.0.1:80"
        );
        assert_eq!(
            rule(ForwardKind::Remote, 8080).label(),
            "R 服务器:8080 → 本机 127.0.0.1:80"
        );
        // 动态转发的目标由客户端逐条协商，描述里不出现目标地址。
        assert_eq!(
            rule(ForwardKind::Dynamic, 1080).label(),
            "D 127.0.0.1:1080（SOCKS5）"
        );
    }

    #[test]
    fn forward_rule_ids_do_not_collide_across_directions() {
        // 本地与远程可以用同一个监听端口，停止时必须按方向区分。
        assert_ne!(
            rule(ForwardKind::Local, 8080).id(),
            rule(ForwardKind::Remote, 8080).id()
        );
        assert_eq!(rule(ForwardKind::Local, 8080).id(), "L-8080");
        assert_eq!(rule(ForwardKind::Remote, 8080).id(), "R-8080");
        assert_eq!(rule(ForwardKind::Dynamic, 8080).id(), "D-8080");
    }

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 起一对本地 socket，让 `socks_handshake` 服务端跑在后台。
    async fn socks_pair() -> (TcpStream, tokio::task::JoinHandle<Result<(String, u16)>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            super::socks_handshake(&mut socket).await
        });
        (TcpStream::connect(addr).await.unwrap(), server)
    }

    async fn greeting(client: &mut TcpStream) {
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
    }

    #[tokio::test]
    async fn socks_handshake_parses_ipv4_connect() {
        let (mut client, server) = socks_pair().await;
        greeting(&mut client).await;
        client
            .write_all(&[0x05, 0x01, 0x00, 0x01, 10, 0, 0, 7, 0x1F, 0x90])
            .await
            .unwrap();
        let target = server.await.unwrap().unwrap();
        assert_eq!(target, ("10.0.0.7".to_string(), 8080));
    }

    #[tokio::test]
    async fn socks_handshake_parses_domain_connect() {
        let (mut client, server) = socks_pair().await;
        greeting(&mut client).await;
        let mut request = vec![0x05, 0x01, 0x00, 0x03, 9];
        request.extend_from_slice(b"localhost");
        request.extend_from_slice(&[0x00, 0x50]);
        client.write_all(&request).await.unwrap();
        let target = server.await.unwrap().unwrap();
        assert_eq!(target, ("localhost".to_string(), 80));
    }

    #[tokio::test]
    async fn socks_handshake_rejects_non_connect_command() {
        let (mut client, server) = socks_pair().await;
        greeting(&mut client).await;
        // BIND（0x02）不支持：必须立刻回 Command not supported，而不是挂住客户端。
        client
            .write_all(&[0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0x00, 0x50])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0x07);
        assert!(server.await.unwrap().is_err());
    }

    use super::{
        CwdKeystrokes, cd_arg_from_line, lex_join, path_from_osc7, percent_decode, reports_cwd,
        shell_path_from_output,
    };

    #[test]
    fn cwd_report_only_targets_bash_and_zsh() {
        assert!(reports_cwd("/bin/bash"));
        assert!(reports_cwd("/usr/local/bin/zsh"));
        // login shell 以 `-bash` 形式启动。
        assert!(reports_cwd("-bash"));
        assert!(!reports_cwd("/usr/bin/fish"));
        assert!(!reports_cwd("/bin/csh"));
        assert!(!reports_cwd("C:\\Windows\\System32\\cmd.exe"));
        assert!(!reports_cwd(""));
    }

    #[test]
    fn login_shell_takes_the_first_absolute_line() {
        assert_eq!(
            shell_path_from_output(b"/bin/bash\n"),
            Some("/bin/bash".into())
        );
        // login shell 以 `-bash` 起，路径形式照样要认。
        assert_eq!(
            shell_path_from_output(b"/usr/local/bin/zsh"),
            Some("/usr/local/bin/zsh".into())
        );
        // Windows 服务器上 `echo $SHELL` 回显字面量；没有输出也没有行。
        assert_eq!(shell_path_from_output(b"$SHELL\n"), None);
        assert_eq!(shell_path_from_output(b"\n"), None);
        let empty: &[u8] = b"";
        assert_eq!(shell_path_from_output(empty), None);
    }

    #[test]
    fn cd_arg_plain_and_quoted() {
        assert_eq!(cd_arg_from_line("cd /tmp"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd  /var/log"), Some("/var/log".into()));
        assert_eq!(cd_arg_from_line("cd"), Some("".into()));
        assert_eq!(cd_arg_from_line("cd ~"), Some("~".into()));
        assert_eq!(cd_arg_from_line("cd ~/src"), Some("~/src".into()));
        assert_eq!(cd_arg_from_line("cd -"), Some("-".into()));
        assert_eq!(
            cd_arg_from_line("cd \"/mnt/c/My Files\""),
            Some("/mnt/c/My Files".into())
        );
        assert_eq!(cd_arg_from_line("cd '/var/log'"), Some("/var/log".into()));
    }

    #[test]
    fn cd_arg_strips_chain_and_options() {
        assert_eq!(cd_arg_from_line("cd /tmp && ls"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd /tmp; pwd"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd /tmp | grep"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd /tmp # comment"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd -L /tmp"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd -L -P /tmp"), Some("/tmp".into()));
    }

    #[test]
    fn cd_arg_rejects_unresolvable() {
        assert_eq!(cd_arg_from_line("ls /tmp"), None);
        assert_eq!(cd_arg_from_line("cd $HOME"), None);
        assert_eq!(cd_arg_from_line("cd `pwd`"), None);
        assert_eq!(cd_arg_from_line("cd *"), None);
        assert_eq!(cd_arg_from_line("cd ~root"), None);
        assert_eq!(cd_arg_from_line("pushd"), None);
        assert_eq!(cd_arg_from_line(""), None);
    }

    #[test]
    fn lex_join_handles_relative_and_dotdot() {
        assert_eq!(lex_join("/home/user", "src"), "/home/user/src");
        assert_eq!(lex_join("/home/user", ".."), "/home");
        assert_eq!(lex_join("/home/user", "../.."), "/");
        assert_eq!(lex_join("/", "a/b"), "/a/b");
        assert_eq!(lex_join("/a/b", "./c"), "/a/b/c");
        assert_eq!(lex_join("/a/b", "c/../d"), "/a/b/d");
    }

    #[test]
    fn percent_decode_handles_utf8_and_invalid() {
        assert_eq!(percent_decode("/tmp"), "/tmp");
        assert_eq!(percent_decode("/a%20b"), "/a b");
        // 中文 UTF-8 路径：`中` = E4 B8 AD
        assert_eq!(percent_decode("/%E4%B8%AD"), "/中");
        // 无效百分号保留原样
        assert_eq!(percent_decode("/a%ZZb"), "/a%ZZb");
        assert_eq!(percent_decode("/a%2"), "/a%2");
    }

    #[test]
    fn path_from_osc7_parses_host_and_empty_host() {
        assert_eq!(
            path_from_osc7("file://server/tmp/logs"),
            Some("/tmp/logs".into())
        );
        assert_eq!(
            path_from_osc7("file:///home/user"),
            Some("/home/user".into())
        );
        assert_eq!(path_from_osc7("file://h/a%20b"), Some("/a b".into()));
        assert_eq!(path_from_osc7("not-a-uri"), None);
        assert_eq!(path_from_osc7("file://host"), None);
    }

    #[test]
    fn keystroke_tracker_detects_cd_and_ignores_escapes() {
        let mut t = CwdKeystrokes::default();
        assert_eq!(t.feed(b"cd /tmp\r"), Some("/tmp".into()));
        // 方向键（ESC）+ 回车：整行不可信
        assert_eq!(t.feed(b"cd /var\x1b[Ar"), None);
        assert_eq!(t.feed(b"\r"), None);
        // 退格正常处理
        assert_eq!(t.feed(b"cd /tmpx\x7f\r"), Some("/tmp".into()));
        // Ctrl+W 删词
        assert_eq!(t.feed(b"cd /tmp /var\x17\r"), Some("/tmp".into()));
        // Ctrl+C 清空
        assert_eq!(t.feed(b"garbage\x03"), None);
        assert_eq!(t.feed(b"cd /etc\r"), Some("/etc".into()));
    }

    /// 真实 SSH 下的目录上报联调（默认跳过）：`cargo test -- --ignored cwd_report`。
    ///
    /// 需要本地测试服务器（Docker 容器 `localhost:2222`，test / test123，OpenSSH + bash）。
    /// 覆盖三段：exec 通道探测 `$SHELL`、把注入行写进交互 shell、确认每个提示符前
    /// 都回一条能被 [`path_from_osc7`] 解析的 OSC 7（换目录后上报的是新目录）。
    #[tokio::test]
    #[ignore = "需要本地 SSH 测试服务器 localhost:2222 (test/test123)"]
    async fn cwd_report_over_real_ssh() {
        use super::{ChannelMsg, SHELL_INTEGRATION, SshConfig};
        use bytes::Bytes;
        use russh::keys::PublicKeyOrCertificate;
        use std::time::Duration;

        struct TrustAll;
        impl russh::client::Handler for TrustAll {
            type Error = russh::Error;
            async fn check_server_key(
                &mut self,
                _key: &PublicKeyOrCertificate,
            ) -> Result<bool, Self::Error> {
                Ok(true)
            }
        }

        let mut handle = russh::client::connect(
            std::sync::Arc::new(SshConfig::default()),
            ("127.0.0.1", 2222u16),
            TrustAll,
        )
        .await
        .expect("测试服务器没起来？先启动容器再跑本测试");
        assert!(
            handle
                .authenticate_password("test", "test123")
                .await
                .is_ok()
        );

        // 探测：sshd 会把 SHELL 写进会话环境，所以 exec 通道读得到登录 shell。
        let probe = handle.channel_open_session().await.unwrap();
        let (mut read, write) = probe.split();
        write.exec(false, b"echo $SHELL".to_vec()).await.unwrap();
        let mut shell = Vec::new();
        while let Some(msg) = read.wait().await {
            match msg {
                ChannelMsg::Data { data } => shell.extend_from_slice(&data),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        drop(write);
        let shell = shell_path_from_output(&shell).unwrap_or_default();
        assert!(reports_cwd(&shell), "ssh 会话里 $SHELL = {shell:?}");

        // 注入：交互 shell 里跑注入行，再 cd，等下行里出现新目录的上报。
        let chan = handle.channel_open_session().await.unwrap();
        chan.request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        chan.request_shell(true).await.unwrap();
        let (mut read, write) = chan.split();
        for command in [format!("{SHELL_INTEGRATION}\r"), "cd /etc\r".to_string()] {
            write.data_bytes(Bytes::from(command)).await.unwrap();
        }
        let mut reported: Vec<String> = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !reported.iter().any(|path| path == "/etc") {
            let msg = tokio::time::timeout_at(deadline, read.wait())
                .await
                .expect("shell 没有上报任何 OSC 7");
            if let Some(ChannelMsg::Data { data }) = msg {
                pending.extend_from_slice(&data);
            }
            // 逐条摘出上报；payload 可能跨数据分片，没等到收尾符就先攒着。
            while let Some(start) = pending.windows(4).position(|w| w == b"\x1b]7;") {
                // BEL 或 ESC(ST) 收尾，两种终止写法都要认。
                let Some(end) = pending[start + 4..]
                    .iter()
                    .position(|&b| b == 0x07 || b == 0x1b)
                else {
                    break;
                };
                let payload =
                    String::from_utf8_lossy(&pending[start + 4..start + 4 + end]).to_string();
                pending = pending[start + 4 + end + 1..].to_vec();
                if let Some(path) = path_from_osc7(&payload) {
                    reported.push(path);
                }
            }
        }
    }
}
