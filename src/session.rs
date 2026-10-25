//! 会话桥接：rgpui-term 外接 IO 后端 ⇄ russh 通道的双向字节流。
//!
//! - 下行：`SshTerminalHandler::data` → `FeedHandle::feed`（见 ssh.rs）
//! - 上行：终端阻塞通道（[`PtyBoundReceiver`]）→ 专用泵线程 →
//!   tokio 无界通道 → 转发任务写入 russh 通道（按键/应答/尺寸变化）
//!
//! 网络 IO 全部在 rgpui 的 tokio 运行时上异步进行，UI 线程零阻塞。

use std::sync::Arc;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use rgpui::AppContext;
use rgpui::Task;
use rgpui_term::{ExternalHandles, FeedHandle, PtyBoundMessage};
use russh::Channel;
use russh::client::{Config as SshConfig, Handle, Msg};
use russh::keys::PrivateKeyWithHashAlg;
use russh::keys::agent::client::AgentClient;
use tokio::sync::mpsc;

use crate::config::SshTarget;
use crate::ssh::SshTerminalHandler;

/// 远端 PTY 的初始尺寸（终端首个 Resize 到达前生效）。
const INIT_COLS: u16 = 80;
const INIT_ROWS: u16 = 24;

/// 客户端会话通道别名。
type SessionChannel = Channel<Msg>;

/// 在 rgpui 的 tokio 运行时上启动 SSH 会话。
///
/// 返回的任务必须被持有（提前 drop 会取消 SSH）。
pub fn start<C: AppContext>(
    target: SshTarget,
    handles: ExternalHandles,
    cx: &C,
) -> Task<Result<()>> {
    let ExternalHandles { feed, rx } = handles;
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<PtyBoundMessage>();

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
    rgpui::tokio::Tokio::spawn_result(cx, async move {
        let result = run(target, feed, ui_rx).await;
        if let Err(error) = &result {
            closing_feed.feed(format!("\r\n[rulink] 会话结束: {error}\r\n").into_bytes());
        }
        // 无论成功失败都以"远端退出"收场，UI 表现与本地 shell 退出一致。
        closing_feed.end_session();
        result
    })
}

/// 建连 → 认证 → 开 PTY/shell → 驱动会话直至断线。
async fn run(
    target: SshTarget,
    feed: FeedHandle,
    ui_rx: mpsc::UnboundedReceiver<PtyBoundMessage>,
) -> Result<()> {
    let config = Arc::new(SshConfig::default());
    let handler = SshTerminalHandler::new(feed.clone(), target.host.clone(), target.port);
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

    // 上行转发：独立任务持有通道与终端消息流；会话断线后
    // data_bytes 会因通道失效而报错，任务自行退出。
    tokio::spawn(forward_uplink(channel, ui_rx));

    // 驱动会话事件循环（Handler 回调在此期间持续收到下行数据）。
    handle.await.context("SSH 会话异常终止")?;
    Ok(())
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
        anyhow::ensure!(ok, "密码认证失败");
        return Ok(());
    }

    if let Some(path) = &target.key_path {
        let secret = russh::keys::load_secret_key(path, None)
            .with_context(|| format!("无法加载私钥 {}", path.display()))?;
        let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
        let key = PrivateKeyWithHashAlg::new(Arc::new(secret), hash_alg);
        let ok = handle
            .authenticate_publickey(target.user.clone(), key)
            .await?
            .success();
        anyhow::ensure!(ok, "公钥认证失败（{}）", path.display());
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
    anyhow::bail!("SSH Agent 中的密钥均被服务器拒绝");
}
