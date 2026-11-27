//! WebSocket 会话：把终端字节跑在 ws://（或 wss://）帧里（issue #3）。
//!
//! 对接目标：websockify（noVNC 后端）、Web 串口网关、嵌入式设备的
//! "浏览器才能开终端"服务——帧内容原样进出，不解释子协议：
//! - 上行按键 → Binary 帧；Resize 无对应概念，忽略；
//! - 下行 Binary / Text 帧的字节 → 终端；Ping 自动回 Pong；
//! - TLS 走 `native-tls`（Windows 上是 schannel），与 reqwest / IronRDP 同栈。
//!
//! 网络 IO 全在 rgpui 的 tokio 运行时上异步进行，UI 线程零阻塞；
//! 下行字节灌入 [`FeedHandle`]，会话结束由 `end_session` 收尾，与 Telnet 一致。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use rgpui::AppContext;
use rgpui_term::{FeedHandle, PtyBoundMessage};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// WebSocket 会话句柄；生命周期契约与 Telnet 完全一致。
pub type WsSession = crate::protocols::SessionHandle;

/// 在 rgpui 的 tokio 运行时上启动 WebSocket 会话。
///
/// `path` / `tls` 来自主机的 [`WsParams`](crate::sessions::WsParams)；
/// 没有参数（主机列表缺省）时路径按 `/`、不走 TLS。
pub fn start<C: AppContext>(
    host: String,
    port: u16,
    path: String,
    tls: bool,
    handles: rgpui_term::ExternalHandles,
    connect_timeout: Duration,
    cx: &C,
) -> WsSession {
    let rgpui_term::ExternalHandles { feed, rx } = handles;
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<PtyBoundMessage>();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (disconnect_tx, disconnect_rx) = oneshot::channel();
    let started = Arc::new(AtomicBool::new(false));

    // 上行泵：把终端的阻塞通道接到 tokio 通道；send 非阻塞，泵线程只阻塞在 recv。
    std::thread::Builder::new()
        .name("ruterm-ws-uplink".to_string())
        .spawn(move || {
            while let Some(message) = rx.recv() {
                if ui_tx.send(message).is_err() {
                    break;
                }
            }
        })
        .expect("无法启动 ruterm WebSocket 上行泵线程");

    let url = build_url(&host, port, &path, tls);
    feed.feed(format!("\r\n[ruterm] 正在连接 WebSocket {url}…\r\n").into_bytes());

    let closing_feed = feed.clone();
    let started_flag = started.clone();
    let join = rgpui::tokio::Tokio::spawn_result(cx, async move {
        let result = run(
            url,
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
                    "\r\n[ruterm] WebSocket 会话结束: {}\r\n",
                    crate::session::error_summary(error)
                )
                .into_bytes(),
            );
        }
        closing_feed.end_session();
        result
    });
    WsSession {
        join,
        started,
        ready: ready_rx,
        disconnect: disconnect_tx,
    }
}

/// 拼接 ws:// / wss:// URL；路径不带前导斜杠时补上，空路径按 `/`。
/// `pub(crate)`：建连层（connect.rs）要用同一个 URL 更新状态栏与重连快照。
pub(crate) fn build_url(host: &str, port: u16, path: &str, tls: bool) -> String {
    let path = path.trim();
    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    format!("{}://{host}:{port}{path}", if tls { "wss" } else { "ws" })
}

/// 建连 → 双向搬运 → 断线。
#[allow(clippy::too_many_arguments)]
async fn run(
    url: String,
    feed: FeedHandle,
    mut up_rx: mpsc::UnboundedReceiver<PtyBoundMessage>,
    started: Arc<AtomicBool>,
    connect_timeout: Duration,
    mut disconnect: oneshot::Receiver<()>,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let (stream, _response) = tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(&url))
        .await
        .map_err(|_| anyhow::anyhow!("连接超时（{} 秒）", connect_timeout.as_secs()))?
        .with_context(|| format!("无法连接 {url}"))?;
    started.store(true, Ordering::SeqCst);
    feed.feed(format!("\r\n[ruterm] 已连接 WebSocket {url}\r\n").into_bytes());
    let _ = ready.send(());

    // 拆成读写两半，select 里才能同时收发。
    let (mut sink, mut stream) = stream.split();
    loop {
        tokio::select! {
            _ = &mut disconnect => {
                feed.feed(format!("\r\n[ruterm] 已主动断开 WebSocket {url}\r\n").into_bytes());
                break;
            }
            message = up_rx.recv() => {
                let Some(message) = message else { break };
                if let PtyBoundMessage::Input(bytes) = message {
                    if bytes.is_empty() {
                        continue;
                    }
                    sink.send(Message::Binary(bytes.into())).await.context("上行写入失败")?;
                }
                // Resize 对 WebSocket 没有通用意义：子协议各说各话，忽略。
            }
            read = stream.next() => {
                let Some(read) = read else {
                    feed.feed(format!("\r\n[ruterm] WebSocket {url} 已关闭连接\r\n").into_bytes());
                    break;
                };
                match read.context("读取失败")? {
                    Message::Binary(data) => {
                        feed.feed(data.to_vec());
                    }
                    Message::Text(text) => {
                        feed.feed(text.as_str().as_bytes().to_vec());
                    }
                    // 服务端探活：原样回 pong，否则网关按空闲超时踢人。
                    Message::Ping(payload) => {
                        sink.send(Message::Pong(payload))
                            .await
                            .context("心跳应答失败")?;
                    }
                    Message::Close(_) => {
                        feed.feed(format!("\r\n[ruterm] WebSocket {url} 已请求关闭\r\n").into_bytes());
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    let _ = sink.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_defaults_path_to_slash() {
        assert_eq!(build_url("h", 80, "", false), "ws://h:80/");
        assert_eq!(build_url("h", 443, "", true), "wss://h:443/");
    }

    #[test]
    fn url_prefixes_leading_slash() {
        assert_eq!(build_url("h", 7681, "ws", false), "ws://h:7681/ws");
        assert_eq!(build_url("h", 7681, "/ws", false), "ws://h:7681/ws");
        assert_eq!(build_url("h", 7681, " /ws ", false), "ws://h:7681/ws");
    }
}
