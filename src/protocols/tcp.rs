//! 裸 TCP 会话：把本机与 `host:port` 之间的字节双向搬运，上面不叠加任何协议。
//!
//! 与 Telnet 唯一的差别是不做 IAC 协商——对端通常是串口服务器（RFC 2217 的
//! 非协商侧）、预留端口的服务或想手动说协议的人；PuTTY 称之为 "Raw" 会话。
//!
//! 网络 IO 全在 rgpui 的 tokio 运行时上异步进行，UI 线程零阻塞；
//! 下行字节灌入 [`FeedHandle`]，会话结束由 `end_session` 收尾，与 Telnet 一致。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use rgpui::AppContext;
use rgpui_term::{FeedHandle, PtyBoundMessage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// 裸 TCP 会话句柄；生命周期契约与 Telnet 完全一致。
pub type TcpSession = crate::protocols::SessionHandle;

/// 在 rgpui 的 tokio 运行时上启动裸 TCP 会话。
pub fn start<C: AppContext>(
    host: String,
    port: u16,
    handles: rgpui_term::ExternalHandles,
    connect_timeout: Duration,
    cx: &C,
) -> TcpSession {
    let rgpui_term::ExternalHandles { feed, rx } = handles;
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<PtyBoundMessage>();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (disconnect_tx, disconnect_rx) = oneshot::channel();
    let started = Arc::new(AtomicBool::new(false));

    // 上行泵：把终端的阻塞通道接到 tokio 通道；send 非阻塞，泵线程只阻塞在 recv。
    std::thread::Builder::new()
        .name("ruterm-tcp-uplink".to_string())
        .spawn(move || {
            while let Some(message) = rx.recv() {
                if ui_tx.send(message).is_err() {
                    break;
                }
            }
        })
        .expect("无法启动 ruterm TCP 上行泵线程");

    feed.feed(format!("\r\n[ruterm] 正在连接 TCP {host}:{port}…\r\n").into_bytes());

    let closing_feed = feed.clone();
    let started_flag = started.clone();
    let join = rgpui::tokio::Tokio::spawn_result(cx, async move {
        let result = run(
            host,
            port,
            feed,
            ui_rx,
            started_flag,
            connect_timeout,
            disconnect_rx,
            ready_tx,
        )
        .await;
        if let Err(error) = &result {
            closing_feed.feed(
                format!(
                    "\r\n[ruterm] TCP 会话结束: {}\r\n",
                    crate::session::error_summary(error)
                )
                .into_bytes(),
            );
        }
        closing_feed.end_session();
        result
    });
    TcpSession {
        join,
        started,
        ready: ready_rx,
        disconnect: disconnect_tx,
    }
}

/// 建连 → 双向搬运 → 断线。
#[allow(clippy::too_many_arguments)]
async fn run(
    host: String,
    port: u16,
    feed: FeedHandle,
    mut up_rx: mpsc::UnboundedReceiver<PtyBoundMessage>,
    started: Arc<AtomicBool>,
    connect_timeout: Duration,
    mut disconnect: oneshot::Receiver<()>,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let address = format!("{host}:{port}");
    let stream = tokio::time::timeout(
        connect_timeout,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("连接超时（{} 秒）", connect_timeout.as_secs()))?
    .with_context(|| format!("无法连接 {address}"))?;
    // 交互会话按小包发送：Nagle 合并会把按键回显拖出可感知的延迟。
    let _ = stream.set_nodelay(true);
    started.store(true, Ordering::SeqCst);
    feed.feed(format!("\r\n[ruterm] 已连接 TCP {address}\r\n").into_bytes());
    let _ = ready.send(());

    let (mut reader, mut writer) = stream.into_split();
    let mut buf = vec![0u8; 8192];
    loop {
        tokio::select! {
            _ = &mut disconnect => {
                feed.feed(format!("\r\n[ruterm] 已主动断开 TCP {address}\r\n").into_bytes());
                break;
            }
            message = up_rx.recv() => {
                let Some(message) = message else { break };
                if let PtyBoundMessage::Input(bytes) = message {
                    if bytes.is_empty() {
                        continue;
                    }
                    writer.write_all(&bytes).await.context("上行写入失败")?;
                }
                // Resize 对裸 TCP 没有意义：没有可协商的窗口尺寸。
            }
            read = reader.read(&mut buf) => {
                let read = read.context("读取失败")?;
                if read == 0 {
                    feed.feed(format!("\r\n[ruterm] TCP {address} 已关闭连接\r\n").into_bytes());
                    break;
                }
                feed.feed(buf[..read].to_vec());
            }
        }
    }
    let _ = writer.shutdown().await;
    Ok(())
}
