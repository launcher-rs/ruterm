//! 串口会话：本地设备（COM3 / /dev/ttyUSB0）上的字节双向搬运（issue #3）。
//!
//! `serialport` 是同步 API，tokio 运行时里没法 select，布局因此与 Telnet 不同：
//! - **读**在专用线程里阻塞进行（超时 100ms），字节经 tokio 通道转回会话任务，
//!   顺带周期性检查停止标记，主动断开最多 100ms 内退出；
//! - **写**用 `try_clone` 出的独立句柄在会话任务里进行（按键小包，驱动缓冲，
//!   受 100ms 超时约束不会久占 tokio 工作线程）；
//! - 两个句柄都 drop 后操作系统才真正关闭设备。
//!
//! 打开设备即拉高 DTR / RTS（与 PuTTY / MobaXterm 一致）；
//! 下行字节灌入 [`FeedHandle`]，会话结束由 `end_session` 收尾。

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use rgpui::AppContext;
use rgpui_term::{FeedHandle, PtyBoundMessage};
use serialport::{DataBits, FlowControl, Parity, StopBits};
use tokio::sync::{mpsc, oneshot};

use crate::sessions::{SerialFlow, SerialParams, SerialParity};

/// 读线程两次轮询停止标记的间隔：串口读超时。也约束写阻塞的上限。
const PORT_TIMEOUT: Duration = Duration::from_millis(100);

/// 串口会话句柄；生命周期契约与 Telnet 完全一致。
pub type SerialSession = crate::protocols::SessionHandle;

/// 在 rgpui 的 tokio 运行时上启动串口会话。
pub fn start<C: AppContext>(
    device: String,
    params: SerialParams,
    handles: rgpui_term::ExternalHandles,
    cx: &C,
) -> SerialSession {
    let rgpui_term::ExternalHandles { feed, rx } = handles;
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<PtyBoundMessage>();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (disconnect_tx, disconnect_rx) = oneshot::channel();
    let started = Arc::new(AtomicBool::new(false));

    // 上行泵：把终端的阻塞通道接到 tokio 通道；send 非阻塞，泵线程只阻塞在 recv。
    std::thread::Builder::new()
        .name("ruterm-serial-uplink".to_string())
        .spawn(move || {
            while let Some(message) = rx.recv() {
                if ui_tx.send(message).is_err() {
                    break;
                }
            }
        })
        .expect("无法启动 ruterm 串口上行泵线程");

    let summary = format!(
        "{device}（{} {}-{}/{}）",
        params.baud,
        params.data_bits,
        match params.parity {
            SerialParity::None => "N",
            SerialParity::Odd => "E",
            SerialParity::Even => "E",
            SerialParity::Mark => "M",
            SerialParity::Space => "S",
        },
        params.stop_bits,
    );
    feed.feed(format!("\r\n[ruterm] 正在打开串口 {summary}…\r\n").into_bytes());

    let closing_feed = feed.clone();
    let started_flag = started.clone();
    let join = rgpui::tokio::Tokio::spawn_result(cx, async move {
        let result = run(device, params, feed, ui_rx, started_flag, disconnect_rx, ready_tx).await;
        if let Err(error) = &result {
            closing_feed.feed(
                format!(
                    "\r\n[ruterm] 串口会话结束: {}\r\n",
                    crate::session::error_summary(error)
                )
                .into_bytes(),
            );
        }
        closing_feed.end_session();
        result
    });
    SerialSession {
        join,
        started,
        ready: ready_rx,
        disconnect: disconnect_tx,
    }
}

/// 打开设备 → 拉起读线程 → 双向搬运 → 断线。
#[allow(clippy::too_many_arguments)]
async fn run(
    device: String,
    params: SerialParams,
    feed: FeedHandle,
    mut up_rx: mpsc::UnboundedReceiver<PtyBoundMessage>,
    started: Arc<AtomicBool>,
    mut disconnect: oneshot::Receiver<()>,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let mut reader = serialport::new(&device, params.baud)
        .data_bits(data_bits(params.data_bits))
        .parity(parity(params.parity))
        .stop_bits(stop_bits(params.stop_bits))
        .flow_control(flow(params.flow))
        .timeout(PORT_TIMEOUT)
        .open()
        .with_context(|| format!("无法打开串口 {device}"))?;
    // 与 PuTTY / MobaXterm 一致：打开即拉高 DTR / RTS。个别驱动不支持时忽略。
    let _ = reader.write_data_terminal_ready(true);
    let _ = reader.write_request_to_send(true);

    let mut writer = reader
        .try_clone()
        .with_context(|| format!("无法复用串口句柄 {device}"))?;
    started.store(true, Ordering::SeqCst);
    feed.feed(format!("\r\n[ruterm] 已打开串口 {device}\r\n").into_bytes());
    let _ = ready.send(());

    // 读线程：阻塞读 → tokio 通道；停止标记让它最多 100ms 退出。
    let stop = Arc::new(AtomicBool::new(false));
    let (down_tx, mut down_rx) = mpsc::unbounded_channel::<Result<Vec<u8>, String>>();
    let read_stop = stop.clone();
    std::thread::Builder::new()
        .name("ruterm-serial-read".to_string())
        .spawn(move || {
            let mut buf = vec![0u8; 8192];
            loop {
                if read_stop.load(Ordering::Relaxed) {
                    break;
                }
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = down_tx.send(Err("设备已断开".to_string()));
                        break;
                    }
                    Ok(n) => {
                        if down_tx.send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    // 读超时是空转的正常路径，继续轮询停止标记。
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(error) => {
                        let _ = down_tx.send(Err(error.to_string()));
                        break;
                    }
                }
            }
        })
        .expect("无法启动 ruterm 串口读线程");

    loop {
        tokio::select! {
            _ = &mut disconnect => {
                feed.feed(format!("\r\n[ruterm] 已关闭串口 {device}\r\n").into_bytes());
                break;
            }
            message = up_rx.recv() => {
                let Some(message) = message else { break };
                if let PtyBoundMessage::Input(bytes) = message {
                    if bytes.is_empty() {
                        continue;
                    }
                    // 同步写：按键小包走驱动缓冲，100ms 超时兜底不会久占工作线程。
                    // Resize 对串口没有意义：波特率固定，没有可协商的窗口尺寸。
                    writer.write_all(&bytes).context("串口写入失败")?;
                }
            }
            read = down_rx.recv() => {
                match read {
                    Some(Ok(data)) => {
                        feed.feed(data);
                    }
                    Some(Err(error)) => anyhow::bail!("串口读取失败：{error}"),
                    // 读线程已退出：正常路径错误已上报，这里兜底收尾。
                    None => break,
                }
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    drop(writer);
    Ok(())
}

fn data_bits(bits: u8) -> DataBits {
    match bits {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        _ => DataBits::Eight,
    }
}

const fn parity(parity: SerialParity) -> Parity {
    match parity {
        SerialParity::Odd => Parity::Odd,
        SerialParity::Even | SerialParity::Mark | SerialParity::Space => Parity::Even,
        SerialParity::None => Parity::None,
    }
}

const fn stop_bits(bits: u8) -> StopBits {
    if bits == 2 {
        StopBits::Two
    } else {
        StopBits::One
    }
}

const fn flow(flow: SerialFlow) -> FlowControl {
    match flow {
        SerialFlow::Software => FlowControl::Software,
        SerialFlow::Hardware => FlowControl::Hardware,
        SerialFlow::None => FlowControl::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_map_to_serialport_types() {
        let default = SerialParams::default();
        assert_eq!(data_bits(default.data_bits), DataBits::Eight);
        assert_eq!(data_bits(5), DataBits::Five);
        // 越界数据位一律落到 8，不 panic。
        assert_eq!(data_bits(9), DataBits::Eight);
        assert_eq!(parity(SerialParity::None), Parity::None);
        assert_eq!(parity(SerialParity::Odd), Parity::Odd);
        assert_eq!(parity(SerialParity::Even), Parity::Even);
        // Mark / Space 只有部分驱动支持，映射到 Even（列进模型但不进表单）。
        assert_eq!(parity(SerialParity::Mark), Parity::Even);
        assert_eq!(parity(SerialParity::Space), Parity::Even);
        assert_eq!(stop_bits(1), StopBits::One);
        assert_eq!(stop_bits(2), StopBits::Two);
        assert_eq!(stop_bits(3), StopBits::One);
        assert_eq!(flow(SerialFlow::None), FlowControl::None);
        assert_eq!(flow(SerialFlow::Software), FlowControl::Software);
        assert_eq!(flow(SerialFlow::Hardware), FlowControl::Hardware);
    }

    #[test]
    fn defaults_are_115200_8n1_without_flow_control() {
        let params = SerialParams::default();
        assert_eq!(params.baud, 115_200);
        assert_eq!(params.data_bits, 8);
        assert_eq!(params.parity, SerialParity::None);
        assert_eq!(params.stop_bits, 1);
        assert_eq!(params.flow, SerialFlow::None);
    }
}
