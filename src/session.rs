//! 会话桥接：rgpui-term 外接 IO 后端 ⇄ russh 通道的双向字节流。
//!
//! - 下行：`SshTerminalHandler::data` → `FeedHandle::feed`（见 ssh.rs）
//! - 上行：终端阻塞通道（[`PtyBoundReceiver`]）→ 专用泵线程 →
//!   tokio 无界通道 → 转发任务写入 russh 通道（按键/应答/尺寸变化）
//!
//! 网络 IO 全部在 rgpui 的 tokio 运行时上异步进行，UI 线程零阻塞。

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
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::config::SshTarget;
use crate::remote::{PROBE_CMD, RemoteInfo};
use crate::ssh::{SideChannels, SshTerminalHandler};

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

/// 一次 SSH 会话：连接主体 + 远程基本信息回传 + 启动标记。
pub struct SshSession {
    /// 会话任务，必须被持有（提前 drop 会取消 SSH）。
    pub join: Task<Result<()>>,
    /// 远程主机基本信息（探测失败/无信息时发送端被丢弃，收到 `Err`）。
    pub remote_info: oneshot::Receiver<RemoteInfo>,
    /// shell 是否成功开启。失败发生在开启之前＝没建立过终端，标签页应保留展示错误。
    pub started: Arc<AtomicBool>,
    /// 主动断开信号：标签被关闭时发送，会话任务随即释放连接。
    pub disconnect: oneshot::Sender<()>,
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
pub fn start<C: AppContext>(
    target: SshTarget,
    handles: ExternalHandles,
    vault: Option<Arc<crate::vault::Vault>>,
    cx: &C,
) -> SshSession {
    let ExternalHandles { feed, rx } = handles;
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<PtyBoundMessage>();
    let (info_tx, info_rx) = oneshot::channel::<RemoteInfo>();
    let (disconnect_tx, disconnect_rx) = oneshot::channel::<()>();
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
    let join = rgpui::tokio::Tokio::spawn_result(cx, async move {
        let result = run(
            target,
            feed,
            ui_rx,
            info_tx,
            started_flag,
            vault,
            disconnect_rx,
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
        disconnect: disconnect_tx,
    }
}

/// 建连 → 认证 → 开 PTY/shell → 探测远程信息 → 驱动会话直至断线。
async fn run(
    target: SshTarget,
    feed: FeedHandle,
    ui_rx: mpsc::UnboundedReceiver<PtyBoundMessage>,
    info_tx: oneshot::Sender<RemoteInfo>,
    started: Arc<AtomicBool>,
    vault: Option<Arc<crate::vault::Vault>>,
    mut disconnect: oneshot::Receiver<()>,
) -> Result<()> {
    let config = Arc::new(SshConfig::default());
    let side = SideChannels::default();
    // Handler 在交互通道 EOF/close/exit-status 时发信号：会话到此为止，
    // 不能继续等 `handle`（TCP 连接还在，会一直挂着，UI 看起来毫无反应）。
    let (ended_tx, mut ended_rx) = mpsc::unbounded_channel::<()>();
    let handler = SshTerminalHandler::new(
        feed.clone(),
        target.host.clone(),
        target.port,
        side.clone(),
        vault,
        ended_tx,
    );
    let mut handle = russh::client::connect(config, (target.host.as_str(), target.port), handler)
        .await
        .with_context(|| format!("无法连接 {}", target.addr()))?;

    authenticate(&mut handle, &target).await?;

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

    // 上行转发先落地：探测通道握手即使被慢服务器拖住，也不能延迟按键转发。
    tokio::spawn(forward_uplink(channel, ui_rx));

    probe_remote_info(&handle, side, info_tx).await;

    // 三条收场路径：远端通道结束、本地关标签、连接本身异常终止。
    // 前两者都主动丢弃 handle，让 SSH 连接随会话一起释放。
    tokio::select! {
        _ = &mut disconnect => log::info!("本地关闭会话 {}", target.addr()),
        _ = ended_rx.recv() => log::info!("远端会话结束 {}", target.addr()),
        // 连接异常终止（网络断开等）时把错误上抛给 UI。
        result = handle => result.context("SSH 会话异常终止")?,
    }
    Ok(())
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

/// 按 密码 → 私钥 → SSH Agent 的优先级完成认证。
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
        let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
        let key = PrivateKeyWithHashAlg::new(Arc::new(secret), hash_alg);
        let ok = handle
            .authenticate_publickey(target.user.clone(), key)
            .await?
            .success();
        if !ok {
            return Err(anyhow::anyhow!(
                "公钥认证被服务器拒绝：{}（请确认此私钥与服务器上授权的公钥匹配；若服务器只允许密码，请把该主机的认证方式改为密码）",
                path.display()
            )
            .context(SecretRejected));
        }
        return Ok(());
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
