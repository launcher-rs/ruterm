//! 裸 TCP 会话：把本机与 `host:port` 之间的字节双向搬运，上面不叠加任何协议。
//!
//! 与 Telnet 唯一的差别是不做 IAC 协商——对端通常是串口服务器（RFC 2217 的
//! 非协商侧）、预留端口的服务或想手动说协议的人；PuTTY 称之为 "Raw" 会话。
//!
//! 网络 IO 全在 rgpui 的 tokio 运行时上异步进行，UI 线程零阻塞；
//! 下行字节灌入 [`LoggingFeed`]（可带会话日志），会话结束由 `end_session` 收尾，与 Telnet 一致。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use rgpui::AppContext;
use rgpui_term::PtyBoundMessage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// 裸 TCP 会话句柄；生命周期契约与 Telnet 完全一致。
pub type TcpSession = crate::protocols::SessionHandle;

/// 在 rgpui 的 tokio 运行时上启动裸 TCP 会话。
///
/// `log` 是会话日志（None = 设置里未开启，`feed` 纯转发）。
pub fn start<C: AppContext>(
    host: String,
    port: u16,
    handles: rgpui_term::ExternalHandles,
    connect_timeout: Duration,
    log: Option<crate::session::SessionLog>,
    cx: &C,
) -> TcpSession {
    let rgpui_term::ExternalHandles { feed, rx } = handles;
    let feed = crate::session::session_log::LoggingFeed::new(feed, log);
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
    feed: crate::session::session_log::LoggingFeed,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 [`start`] 一致的 run 入参包装：真实 FeedHandle（解析线程活着，
    /// feed 不会因无接收端而丢）套上日志（测试按需传 Some/None）+ 测试用上行
    /// 通道（无 Terminal 实体时 PtyBoundReceiver 注入不进来，上行走测试通道
    /// 直接驱动 run 的写路径）。
    fn handles(
        log: Option<crate::session::SessionLog>,
    ) -> (
        crate::session::session_log::LoggingFeed,
        mpsc::UnboundedSender<PtyBoundMessage>,
        mpsc::UnboundedReceiver<PtyBoundMessage>,
    ) {
        let mut builder = rgpui_term::TerminalBuilder::new_external(None).expect("外联后端");
        let rgpui_term::ExternalHandles { feed, .. } =
            builder.take_external_handles().expect("外联句柄");
        let (up_tx, up_rx) = mpsc::unbounded_channel();
        (crate::session::session_log::LoggingFeed::new(feed, log), up_tx, up_rx)
    }

    /// 起一个记录收字节的回显服务器：连上先推一条欢迎语（确定性的下行样本），
    /// 之后收到的每段字节先上报再原样回写。
    async fn echo_server() -> (u16, mpsc::UnboundedReceiver<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (got_tx, got_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("接受连接");
            if sock.write_all(b"welcome\r\n").await.is_err() {
                return;
            }
            loop {
                let mut buf = vec![0u8; 1024];
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.truncate(n);
                        if got_tx.send(buf.clone()).is_err() {
                            break;
                        }
                        if sock.write_all(&buf).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        (port, got_rx)
    }

    #[tokio::test]
    async fn run_roundtrips_bytes_against_a_real_echo_server() {
        let (port, mut got_rx) = echo_server().await;
        let (feed, up_tx, up_rx) = handles(None);
        let started = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let (disconnect_tx, disconnect_rx) = oneshot::channel();

        let task = tokio::spawn(run(
            "127.0.0.1".to_string(),
            port,
            feed,
            up_rx,
            started.clone(),
            Duration::from_secs(5),
            disconnect_rx,
            ready_tx,
        ));

        // 建连成功：ready 触发、started 置位。
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .expect("ready 超时")
            .expect("ready 关闭");
        assert!(started.load(Ordering::SeqCst));

        // 上行：注入的字节必须原样到达服务器。
        up_tx
            .send(PtyBoundMessage::Input(b"ping\r\n".to_vec()))
            .unwrap();
        let received = tokio::time::timeout(Duration::from_secs(5), got_rx.recv())
            .await
            .expect("等服务器收字节超时")
            .expect("服务器通道关闭");
        assert_eq!(received, b"ping\r\n");

        // 主动断开：run 顺利收尾。
        disconnect_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("run 未在断开后退出")
            .expect("join 失败");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_survives_server_side_close() {
        // 服务器收一段就关闭连接：run 应当以 Ok 退出（远端关闭不是错误）。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64];
            if sock.read(&mut buf).await.is_ok() {
                // 收到即断，不做回显。
            }
        });
        let (feed, _up_tx, _up_rx) = handles(None);
        let started = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let (disconnect_tx, disconnect_rx) = oneshot::channel();
        let (up_fwd_tx, up_fwd_rx) = mpsc::unbounded_channel();

        let task = tokio::spawn(run(
            "127.0.0.1".to_string(),
            port,
            feed,
            up_fwd_rx,
            started.clone(),
            Duration::from_secs(5),
            disconnect_rx,
            ready_tx,
        ));
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .expect("ready 超时")
            .expect("ready 关闭");
        // 触发服务器读取后关闭。
        up_fwd_tx
            .send(PtyBoundMessage::Input(b"bye".to_vec()))
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("run 未在服务器关闭后退出")
            .expect("join 失败");
        assert!(result.is_ok());
        drop(disconnect_tx);
    }

    #[tokio::test]
    async fn run_times_out_on_unreachable_port() {
        // 监听后立刻关闭 listener：端口无人接受连接，建连在超时内失败。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (feed, _up_tx, up_rx) = handles(None);
        let started = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let (disconnect_tx, disconnect_rx) = oneshot::channel();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run(
                "127.0.0.1".to_string(),
                port,
                feed,
                up_rx,
                started.clone(),
                Duration::from_millis(300),
                disconnect_rx,
                ready_tx,
            ),
        )
        .await
        .expect("超时路径本身卡住");
        assert!(result.is_err(), "无人监听应报错");
        assert!(!started.load(Ordering::SeqCst), "未建立连接不该置位");
        drop(disconnect_tx);
        drop(ready_rx);
    }

    #[tokio::test]
    async fn run_writes_downlink_bytes_to_the_session_log() {
        // 会话日志开启：服务器回显的字节（下行）必须原样落盘；会话结束（句柄
        // 全部 drop）后写线程冲刷收尾。
        let dir = std::env::temp_dir().join(format!(
            "ruterm-tcp-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let log =
            crate::session::session_log::SessionLog::open(&dir, "tcp-log-test").expect("打开日志");

        let (port, mut got_rx) = echo_server().await;
        let (feed, up_tx, up_rx) = handles(Some(log));
        let started = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let (disconnect_tx, disconnect_rx) = oneshot::channel();

        let task = tokio::spawn(run(
            "127.0.0.1".to_string(),
            port,
            feed,
            up_rx,
            started.clone(),
            Duration::from_secs(5),
            disconnect_rx,
            ready_tx,
        ));
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .expect("ready 超时")
            .expect("ready 关闭");
        // 上行也验一份：服务器必须收到我们注入的字节。
        up_tx
            .send(PtyBoundMessage::Input(b"log-me\r\n".to_vec()))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), got_rx.recv())
            .await
            .expect("等服务器收字节超时")
            .expect("服务器通道关闭");
        // 给回显留一个往返的时间窗，避免断开在下行被读走之前抢先命中 select。
        tokio::time::sleep(Duration::from_millis(200)).await;
        disconnect_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("run 未退出")
            .expect("join 失败")
            .expect("run 出错");
        drop(up_tx);

        // 日志里应有欢迎语（连接即推的确定性下行样本）与回显字节；
        // 句柄全部 drop 后写线程才冲刷，轮询等它落盘。
        let contains = |bytes: &[u8], needle: &[u8]| {
            bytes.windows(needle.len()).any(|window| window == needle)
        };
        let mut content: Option<Vec<u8>> = None;
        for _ in 0..200 {
            let files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
                .expect("日志目录")
                .map(|entry| entry.unwrap().path())
                .collect();
            if files.len() == 1 {
                let bytes = std::fs::read(&files[0]).unwrap_or_default();
                if contains(&bytes, b"welcome\r\n") && contains(&bytes, b"log-me\r\n") {
                    content = Some(bytes);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            content.is_some(),
            "日志文件应同时包含欢迎语与回显字节（下行落盘）"
        );
        // 上行按键绝不落盘：注入的 Input 只会以远端回显的形式出现。
        let bytes = content.unwrap();
        let count = |bytes: &[u8], needle: &[u8]| {
            bytes
                .windows(needle.len())
                .filter(|window| *window == needle)
                .count()
        };
        assert_eq!(
            count(&bytes, b"log-me\r\n"),
            1,
            "log-me 只应出现一次（远端回显），不存在第二条输入记录"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
