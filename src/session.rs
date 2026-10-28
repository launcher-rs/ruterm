//! 会话桥接：rgpui-term 外接 IO 后端 ⇄ russh 通道的双向字节流。
//!
//! - 下行：`SshTerminalHandler::data` → `FeedHandle::feed`（见 ssh.rs）
//! - 上行：终端阻塞通道（[`PtyBoundReceiver`]）→ 专用泵线程 →
//!   tokio 无界通道 → 转发任务写入 russh 通道（按键/应答/尺寸变化）
//!
//! 网络 IO 全部在 rgpui 的 tokio 运行时上异步进行，UI 线程零阻塞。

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use rgpui::AppContext;
use rgpui::Task;
use rgpui_term::{ExternalHandles, FeedHandle, PtyBoundMessage};
use russh::Channel;
use russh::ChannelMsg;
use russh::client::{Config as SshConfig, Handle, Msg};
use russh::keys::PrivateKeyWithHashAlg;
use russh::keys::agent::client::AgentClient;
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

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

/// 远程信息探测的整体超时：远端禁用 exec 或命令挂死时放弃，状态栏不显示该项。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// 客户端会话通道别名。
type SessionChannel = Channel<Msg>;

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

/// 一条生效中的转发：规则 + 监听任务（仅本地转发）+ 已建立连接的双向泵。
///
/// 停止转发要连活动连接一起掐断，不能让"停止"只关监听、流量照旧。
struct ActiveForward {
    rule: ForwardRule,
    listener: Option<JoinHandle<()>>,
    pumps: JoinSet<()>,
}

/// UI 发给会话任务的指令：SFTP 与端口转发都必须借用同一条已认证的 SSH 连接。
pub enum SessionCommand {
    /// 读取远端主目录，用于文件传输弹窗预填路径。
    Home {
        reply: oneshot::Sender<Result<String>>,
    },
    /// 执行一次上传 / 下载。
    Transfer(transfer::Task),
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

/// 一次 SSH 会话：连接主体 + 远程基本信息回传 + 启动标记。
pub struct SshSession {
    /// 会话任务，必须被持有（提前 drop 会取消 SSH）。
    pub join: Task<Result<()>>,
    /// 远程主机基本信息（探测失败/无信息时发送端被丢弃，收到 `Err`）。
    pub remote_info: oneshot::Receiver<RemoteInfo>,
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
    cx: &C,
) -> SshSession {
    let ExternalHandles { feed, rx } = handles;
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<PtyBoundMessage>();
    let (info_tx, info_rx) = oneshot::channel::<RemoteInfo>();
    let (ready_tx, ready_rx) = oneshot::channel::<()>();
    let (disconnect_tx, disconnect_rx) = oneshot::channel::<()>();
    let (command_tx, command_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let started = Arc::new(AtomicBool::new(false));

    // 上行泵：把终端的阻塞通道接到 tokio 通道；send 是非阻塞的，
    // 泵线程只在 recv 上休眠，不碰 UI 也不占运行时。
    std::thread::Builder::new()
        .name("rulink-uplink".to_string())
        .spawn(move || {
            while let Some(message) = rx.recv() {
                if ui_tx.send(message).is_err() {
                    break; // 转发任务已结束
                }
            }
        })
        .expect("无法启动 rulink 上行泵线程");

    feed.feed(format!("\r\n[rulink] 正在连接 {}…\r\n", target.addr()).into_bytes());

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
            started_flag,
            vault,
            disconnect_rx,
            command_rx,
            command_keepalive,
            connect_timeout,
            ready_tx,
        )
        .await;
        if let Err(error) = &result {
            closing_feed
                .feed(format!("\r\n[rulink] 会话结束: {}\r\n", error_summary(error)).into_bytes());
        }
        // 无论成功失败都以"远端退出"收场，UI 表现与本地 shell 退出一致。
        closing_feed.end_session();
        result
    });
    SshSession {
        join,
        remote_info: info_rx,
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
    info_tx: oneshot::Sender<RemoteInfo>,
    started: Arc<AtomicBool>,
    vault: Option<Arc<crate::vault::Vault>>,
    mut disconnect: oneshot::Receiver<()>,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    _command_keepalive: mpsc::UnboundedSender<SessionCommand>,
    connect_timeout: Duration,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    // 远程转发的通道由 Handler 接住后递到这里：会话循环统一拨本机目标并搬运字节。
    let (forwarded_tx, mut forwarded_rx) = mpsc::unbounded_channel::<ForwardedTcpip>();
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
            feed.feed(format!("\r\n[rulink] 经跳板机连接：{path}\r\n").into_bytes());
        }
        // 跳板机自己的通道结束 / forwarded 信号与主会话无关：接收端马上丢弃，
        // 发送失败由 `let _ =` 吞掉。
        let (jump_ended_tx, _jump_ended_rx) = mpsc::unbounded_channel::<()>();
        let (jump_forwarded_tx, _jump_forwarded_rx) = mpsc::unbounded_channel::<ForwardedTcpip>();

        // 已建立的跳板机连接必须活到会话结束（句柄一丢，通道随之关闭），
        // 所以收集在这里、最后连同主会话句柄一起带出 setup。
        let mut hops: Vec<(Handle<SshTerminalHandler>, SideChannels)> = Vec::new();
        let nodes: Vec<&SshTarget> = chain.iter().chain(std::iter::once(&target)).collect();
        for (index, node) in nodes.iter().enumerate() {
            let is_target = index + 1 == nodes.len();
            let config = Arc::new(SshConfig::default());
            let node_side = if is_target { side.clone() } else { SideChannels::default() };
            let handler = SshTerminalHandler::new(
                feed.clone(),
                node.host.clone(),
                node.port,
                node_side.clone(),
                vault.clone(),
                if is_target { ended_tx.clone() } else { jump_ended_tx.clone() },
                if is_target { forwarded_tx.clone() } else { jump_forwarded_tx.clone() },
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
                        .with_context(|| {
                            format!("跳板机 {hop} 无法转发到 {}", node.addr())
                        })?;
                    // 通道必须登记进跳板机的旁路集合，否则字节会灌进终端。
                    previous_side.add(channel.id());
                    russh::client::connect_stream(config, channel.into_stream(), handler)
                        .await
                        .with_context(|| {
                            format!("经跳板机 {hop} 连接 {} 失败", node.addr())
                        })?
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

    // 上行转发先落地：探测通道握手即使被慢服务器拖住，也不能延迟按键转发。
    tokio::spawn(forward_uplink(channel, ui_rx));

    probe_remote_info(&handle, side.clone(), info_tx).await;

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
                        let result = match open_sftp(&handle, &side).await {
                            Ok(sftp) => {
                                let home = transfer::remote_home(&sftp).await;
                                let _ = sftp.close().await;
                                home
                            }
                            Err(error) => Err(error),
                        };
                        let _ = reply.send(result);
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
        if let Some(listener) = forward.listener.take() {
            listener.abort();
        }
        forward.pumps.shutdown().await;
    }
    for (_, mut forward) in remote_forwards {
        forward.pumps.shutdown().await;
    }
    for (_, mut forward) in dynamic_forwards {
        if let Some(listener) = forward.listener.take() {
            listener.abort();
        }
        forward.pumps.shutdown().await;
    }
    Ok(())
}

/// 停止一条转发：先掐监听（本地转发）再收掉该规则的全部活动连接。
async fn stop_forward_task(task: &mut Option<ActiveForward>) -> bool {
    let Some(forward) = task else {
        return false;
    };
    if let Some(listener) = forward.listener.take() {
        listener.abort();
    }
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
    let listener = tokio::spawn(forward_accept_loop(
        listener,
        accepted.clone(),
        rule.clone(),
    ));
    Ok(ActiveForward {
        rule,
        listener: Some(listener),
        pumps: JoinSet::new(),
    })
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
        .with_context(|| {
            format!(
                "无法监听 SOCKS5 端口 {}（可能已被占用）",
                rule.listen_port
            )
        })?;
    let listener = tokio::spawn(dynamic_accept_loop(
        listener,
        accepted.clone(),
        rule.clone(),
    ));
    Ok(ActiveForward {
        rule,
        listener: Some(listener),
        pumps: JoinSet::new(),
    })
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
            listener: None,
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
        SftpSession::new(channel.into_stream())
            .await
            .context("SFTP 会话初始化失败")
    };
    tokio::time::timeout(SFTP_TIMEOUT, open)
        .await
        .map_err(|_| anyhow::anyhow!("SFTP 通道超时（服务器未响应）"))?
}

/// 另开一条 exec 通道采集远程主机基本信息，结果异步回传状态栏。
///
/// 探测失败（远端禁用 exec、通道打不开、命令超时无输出）只记日志，
/// 不影响已建立的交互会话。
async fn probe_remote_info(
    handle: &Handle<SshTerminalHandler>,
    side: SideChannels,
    info_tx: oneshot::Sender<RemoteInfo>,
) {
    let Ok(probe) = handle.channel_open_session().await else {
        log::debug!("无法开启远程信息探测通道");
        return;
    };
    side.add(probe.id());
    let (mut read, write) = probe.split();
    if write
        .exec(false, PROBE_CMD.as_bytes().to_vec())
        .await
        .is_err()
    {
        log::debug!("远程信息探测命令下发失败");
        return;
    }

    tokio::spawn(async move {
        let collected = tokio::time::timeout(PROBE_TIMEOUT, async {
            let mut buf = Vec::new();
            while let Some(msg) = read.wait().await {
                match msg {
                    ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                        buf.extend_from_slice(&data);
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    _ => {}
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
        .await;
        let _ = write.close().await;

        match collected {
            Ok(raw) => {
                let info = RemoteInfo::parse(&raw);
                if !info.is_empty() {
                    let _ = info_tx.send(info);
                }
            }
            Err(_) => log::debug!("远程信息探测超时，跳过状态栏展示"),
        }
    });
}

/// 终端 → 远端：按键字节写通道，尺寸变化发 window_change。
async fn forward_uplink(
    channel: SessionChannel,
    mut ui_rx: mpsc::UnboundedReceiver<PtyBoundMessage>,
) {
    while let Some(message) = ui_rx.recv().await {
        let sent = match message {
            PtyBoundMessage::Input(bytes) => channel.data_bytes(Bytes::from(bytes)).await,
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
        };
        if sent.is_err() {
            log::debug!("上行通道已失效，结束转发");
            return;
        }
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
        let pipe = std::env::var_os("SSH_AUTH_SOCK")
            .context("未提供密码/私钥，且 SSH_AUTH_SOCK 未设置，无法使用 SSH Agent")?;
        AgentClient::connect_named_pipe(pipe)
            .await
            .context("无法连接 SSH Agent 命名管道")?
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
    return Err(anyhow::anyhow!("SSH Agent 中的密钥均被服务器拒绝").context(SecretRejected));
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
}
