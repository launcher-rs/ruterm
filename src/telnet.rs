//! Telnet 会话：明文 TCP + 最小 IAC（RFC 854 / 857 / 858）协商。
//!
//! 只实现交互式终端必需的部分：
//! - 接受 ECHO / SGA（服务端回显、抑制 GA），其余 WILL/DO 一律 DONT/WONT；
//! - 丢弃服务端子协商（SB … SE），不回内容，终端里不会出现协商垃圾；
//! - 主动协商 NAWS，把终端行列数告知服务端（BBS / 全屏程序据此排版）；
//! - 上行字节里的 0xFF 按 Telnet 规则双写转义。
//!
//! 网络 IO 全在 rgpui 的 tokio 运行时上异步进行，UI 线程零阻塞；
//! 下行字节灌入 [`FeedHandle`]，会话结束由 `end_session` 收尾，与 SSH 一致。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use rgpui::AppContext;
use rgpui::Task;
use rgpui_term::{ExternalHandles, FeedHandle, PtyBoundMessage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// Telnet 命令字节。
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

/// 选项号。
const TELOPT_ECHO: u8 = 1;
const TELOPT_SGA: u8 = 3;
const TELOPT_NAWS: u8 = 31;

/// Telnet 会话句柄（与 SSH 会话同形，UI 侧统一按"可重连会话"处理）。
pub struct TelnetSession {
    /// 会话任务，必须被持有（提前 drop 会中断连接）。
    pub join: Task<Result<()>>,
    /// 是否成功建立过连接（失败发生在建连阶段时标签按"连接失败"处理）。
    pub started: Arc<AtomicBool>,
    /// TCP 建连成功时发送一次：UI 据此记录"连接真正可用"的时刻。
    pub ready: oneshot::Receiver<()>,
    /// 主动断开信号：标签被关闭时发送，会话任务随即释放连接。
    pub disconnect: oneshot::Sender<()>,
}

/// 在 rgpui 的 tokio 运行时上启动 Telnet 会话。
pub fn start<C: AppContext>(
    host: String,
    port: u16,
    handles: ExternalHandles,
    connect_timeout: Duration,
    cx: &C,
) -> TelnetSession {
    let ExternalHandles { feed, rx } = handles;
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<PtyBoundMessage>();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (disconnect_tx, disconnect_rx) = oneshot::channel();
    let started = Arc::new(AtomicBool::new(false));

    // 上行泵：把终端的阻塞通道接到 tokio 通道；send 非阻塞，泵线程只阻塞在 recv。
    std::thread::Builder::new()
        .name("ruterm-telnet-uplink".to_string())
        .spawn(move || {
            while let Some(message) = rx.recv() {
                if ui_tx.send(message).is_err() {
                    break;
                }
            }
        })
        .expect("无法启动 ruterm Telnet 上行泵线程");

    feed.feed(format!("\r\n[ruterm] 正在连接 Telnet {host}:{port}…\r\n").into_bytes());

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
                format!("\r\n[ruterm] Telnet 会话结束: {}\r\n", crate::session::error_summary(error))
                    .into_bytes(),
            );
        }
        closing_feed.end_session();
        result
    });
    TelnetSession {
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
    feed.feed(format!("\r\n[ruterm] 已连接 Telnet {address}\r\n").into_bytes());
    let _ = ready.send(());

    let (mut reader, mut writer) = stream.into_split();
    let mut filter = IacFilter::default();
    let mut buf = vec![0u8; 8192];
    loop {
        tokio::select! {
            _ = &mut disconnect => {
                feed.feed(format!("\r\n[ruterm] 已主动断开 Telnet {address}\r\n").into_bytes());
                break;
            }
            message = up_rx.recv() => {
                let Some(message) = message else { break };
                match message {
                    PtyBoundMessage::Input(bytes) => {
                        if bytes.is_empty() {
                            continue;
                        }
                        let escaped = escape_uplink(&bytes);
                        writer
                            .write_all(&escaped)
                            .await
                            .context("上行写入失败")?;
                    }
                    PtyBoundMessage::Resize(size) => {
                        filter.set_size(size.num_cols, size.num_lines);
                        let replies = filter.take_replies();
                        if !replies.is_empty() {
                            writer
                                .write_all(&replies)
                                .await
                                .context("协商写入失败")?;
                        }
                    }
                }
            }
            read = reader.read(&mut buf) => {
                let read = read.context("读取失败")?;
                if read == 0 {
                    feed.feed(format!("\r\n[ruterm] Telnet {address} 已关闭连接\r\n").into_bytes());
                    break;
                }
                let data = filter.process(&buf[..read]);
                if !data.is_empty() {
                    feed.feed(data);
                }
                let replies = filter.take_replies();
                if !replies.is_empty() {
                    writer
                        .write_all(&replies)
                        .await
                        .context("协商写入失败")?;
                }
            }
        }
    }
    let _ = writer.shutdown().await;
    Ok(())
}

/// 上行转义：数据里的 0xFF 必须双写，否则对端会当成命令起始。
fn escape_uplink(bytes: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(bytes.len() + 4);
    for &byte in bytes {
        escaped.push(byte);
        if byte == IAC {
            escaped.push(IAC);
        }
    }
    escaped
}

/// IAC 过滤器的解析状态（跨多次 read 保持）。
#[derive(Default)]
enum IacState {
    #[default]
    Data,
    Iac,
    /// 已读到 WILL/WONT/DO/DONT，等待选项号。
    Verb(u8),
    /// 子协商内容：整段丢弃。
    Subneg,
    /// 子协商里读到 IAC，等待它是不是 SE。
    SubnegIac,
}

/// 从下行字节流里剥掉 Telnet 命令，收集需要回给服务端的应答。
#[derive(Default)]
struct IacFilter {
    state: IacState,
    replies: Vec<u8>,
    size: Option<(u16, u16)>,
    /// 已发出 WILL NAWS（避免重复声明）。
    naws_sent: bool,
    /// 服务端明确拒绝 NAWS 后不再发送窗口尺寸。
    naws_rejected: bool,
}

impl IacFilter {
    /// 处理一段下行数据，返回应写进终端的纯数据。
    fn process(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &byte in input {
            match self.state {
                IacState::Data => {
                    if byte == IAC {
                        self.state = IacState::Iac;
                    } else {
                        out.push(byte);
                    }
                }
                IacState::Iac => match byte {
                    // IAC IAC 是数据里的字面 0xFF。
                    IAC => {
                        out.push(IAC);
                        self.state = IacState::Data;
                    }
                    WILL | WONT | DO | DONT => self.state = IacState::Verb(byte),
                    SB => self.state = IacState::Subneg,
                    // NOP / GA 等单字节命令：吞掉。
                    _ => self.state = IacState::Data,
                },
                IacState::Verb(verb) => {
                    self.negotiate(verb, byte);
                    self.state = IacState::Data;
                }
                IacState::Subneg => {
                    if byte == IAC {
                        self.state = IacState::SubnegIac;
                    }
                }
                IacState::SubnegIac => {
                    self.state = if byte == SE {
                        IacState::Data
                    } else {
                        IacState::Subneg
                    };
                }
            }
        }
        out
    }

    /// 最小协商：ECHO / SGA / NAWS 接受，其余拒绝；对端先拒绝时不再应答。
    fn negotiate(&mut self, verb: u8, option: u8) {
        match (verb, option) {
            (WILL, TELOPT_ECHO) | (WILL, TELOPT_SGA) => {
                self.replies.extend([IAC, DO, option]);
            }
            (WILL, _) => self.replies.extend([IAC, DONT, option]),
            (DO, TELOPT_SGA) => self.replies.extend([IAC, WILL, option]),
            (DO, TELOPT_NAWS) => {
                self.announce_naws();
                self.push_naws();
            }
            (DO, _) => self.replies.extend([IAC, WONT, option]),
            (WONT, TELOPT_NAWS) | (DONT, TELOPT_NAWS) => {
                self.naws_rejected = true;
            }
            // 对端拒绝其它选项：无需回应。
            (WONT, _) | (DONT, _) => {}
            _ => {}
        }
    }

    /// 记录终端尺寸；首次知道尺寸时主动声明 WILL NAWS，之后每次都同步。
    fn set_size(&mut self, cols: u16, rows: u16) {
        self.size = Some((cols.max(1), rows.max(1)));
        self.announce_naws();
        self.push_naws();
    }

    fn announce_naws(&mut self) {
        if self.naws_sent || self.naws_rejected {
            return;
        }
        self.naws_sent = true;
        self.replies.extend([IAC, WILL, TELOPT_NAWS]);
    }

    /// NAWS 子协商：`IAC SB NAWS <16 位宽> <16 位高> IAC SE`（网络字节序）。
    fn push_naws(&mut self) {
        if !self.naws_sent || self.naws_rejected {
            return;
        }
        let Some((cols, rows)) = self.size else {
            return;
        };
        let [cols_hi, cols_lo] = cols.to_be_bytes();
        let [rows_hi, rows_lo] = rows.to_be_bytes();
        self.replies
            .extend([IAC, SB, TELOPT_NAWS, cols_hi, cols_lo, rows_hi, rows_lo, IAC, SE]);
    }

    fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_data_passes_through() {
        let mut filter = IacFilter::default();
        assert_eq!(filter.process(b"hello\r\n"), b"hello\r\n");
        assert!(filter.take_replies().is_empty());
    }

    #[test]
    fn escaped_iac_decodes_to_literal_255() {
        let mut filter = IacFilter::default();
        assert_eq!(filter.process(&[0x41, IAC, IAC, 0x42]), vec![0x41, 0xFF, 0x42]);
    }

    #[test]
    fn accepts_echo_and_sga_rejects_others() {
        let mut filter = IacFilter::default();
        filter.process(&[IAC, WILL, TELOPT_ECHO]);
        assert_eq!(filter.take_replies(), vec![IAC, DO, TELOPT_ECHO]);
        filter.process(&[IAC, WILL, TELOPT_SGA]);
        assert_eq!(filter.take_replies(), vec![IAC, DO, TELOPT_SGA]);
        filter.process(&[IAC, WILL, 24]);
        assert_eq!(filter.take_replies(), vec![IAC, DONT, 24]);
        filter.process(&[IAC, DO, 24]);
        assert_eq!(filter.take_replies(), vec![IAC, WONT, 24]);
        filter.process(&[IAC, DO, TELOPT_SGA]);
        assert_eq!(filter.take_replies(), vec![IAC, WILL, TELOPT_SGA]);
        // 对端拒绝不需要回应，也不会再被当成数据。
        filter.process(&[IAC, WONT, TELOPT_ECHO]);
        assert!(filter.take_replies().is_empty());
    }

    #[test]
    fn subnegotiation_is_dropped_including_escaped_iac() {
        let mut filter = IacFilter::default();
        let bytes = [
            0x61, IAC, SB, 24, 0x00, IAC, IAC, 0x01, IAC, SE, 0x62,
        ];
        assert_eq!(filter.process(&bytes), vec![b'a', b'b']);
        assert!(filter.take_replies().is_empty());
    }

    #[test]
    fn state_survives_chunk_boundaries() {
        let mut filter = IacFilter::default();
        assert_eq!(filter.process(&[0x61, IAC]), vec![b'a']);
        // 跨包：命令被拆成两段，仍应整体吞掉；后面的数据正常输出。
        assert_eq!(filter.process(&[WILL, 24, 0x62]), vec![b'b']);
        assert_eq!(filter.take_replies(), vec![IAC, DONT, 24]);
        // 子协商跨包。
        assert_eq!(filter.process(&[IAC, SB, 31, 0x00]), Vec::<u8>::new());
        assert_eq!(filter.process(&[0x50, IAC, SE, 0x63]), vec![b'c']);
    }

    #[test]
    fn size_announces_and_sends_naws() {
        let mut filter = IacFilter::default();
        filter.set_size(120, 40);
        let replies = filter.take_replies();
        assert_eq!(
            replies,
            vec![
                IAC, WILL, TELOPT_NAWS,
                IAC, SB, TELOPT_NAWS, 0x00, 120, 0x00, 40, IAC, SE,
            ]
        );
        // 再次变化只发子协商，不重复 WILL。
        filter.set_size(100, 30);
        assert_eq!(
            filter.take_replies(),
            vec![IAC, SB, TELOPT_NAWS, 0x00, 100, 0x00, 30, IAC, SE]
        );
    }

    #[test]
    fn server_requests_naws_before_size_is_known() {
        let mut filter = IacFilter::default();
        filter.process(&[IAC, DO, TELOPT_NAWS]);
        assert_eq!(filter.take_replies(), vec![IAC, WILL, TELOPT_NAWS]);
        // 尺寸一到位就补发子协商。
        filter.set_size(80, 24);
        assert_eq!(
            filter.take_replies(),
            vec![IAC, SB, TELOPT_NAWS, 0x00, 80, 0x00, 24, IAC, SE]
        );
    }

    #[test]
    fn rejected_naws_stops_sending_size() {
        let mut filter = IacFilter::default();
        filter.process(&[IAC, DONT, TELOPT_NAWS]);
        filter.set_size(80, 24);
        assert!(filter.take_replies().is_empty());
    }

    #[test]
    fn uplink_escapes_literal_255() {
        assert_eq!(escape_uplink(&[0x01, 0xFF, 0x02]), vec![0x01, 0xFF, 0xFF, 0x02]);
    }
}
