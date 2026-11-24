//! ZMODEM 发送端：配合远端 `rz`（lrzsz）把本地文件上传到 shell 当前目录。
//!
//! 上行字节走独立的原始通道（不经过终端 IO），下行由 [`crate::protocols::ssh::ZmodemSink`]
//! 在握手期间接管；协议状态机由 `zmodem2` 驱动，本模块只负责文件读写、超时与进度。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zmodem2::{Action, Event, FileInfo, Position, Sender};

use crate::transfer;

/// 发出 `rz` 后等第一段 ZMODEM 应答的上限：超过基本可判定远端没有 rz。
const FIRST_RESPONSE: Duration = Duration::from_secs(15);
/// 握手完成后两段应答之间的最长空闲。
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 空闲等待切片：定时调用 `timeout()` 重发握手，又不能刷得太密。
const TICK: Duration = Duration::from_secs(3);
/// ZDLE 字节：ZMODEM 帧定界与控制字节转义的起始（CAN，0x18）。shell 的
/// 回显与报错文本永远不含它，以此区分「协议流量」和「普通终端输出」。
const ZDLE: u8 = 0x18;

/// 通过 ZMODEM 上传一个文件；`remote_name` 只允许是文件名（rz 写进当前目录）。
/// `cancel` 置位后每轮循环检查，尽快收场并返回「已取消」错误。
pub async fn send_file(
    path: &Path,
    remote_name: &str,
    downlink: &mut UnboundedReceiver<Vec<u8>>,
    uplink: &UnboundedSender<Vec<u8>>,
    progress: &UnboundedSender<(u64, u64)>,
    cancel: &AtomicBool,
) -> Result<transfer::Summary> {
    let started = Instant::now();
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("无法读取本地文件 {}", path.display()))?;
    let metadata = file
        .metadata()
        .await
        .with_context(|| format!("无法读取本地文件信息 {}", path.display()))?;
    if metadata.is_dir() {
        anyhow::bail!("本地路径是目录：{}", path.display());
    }
    let size = metadata.len();
    if size > u64::from(u32::MAX) {
        anyhow::bail!("文件超过 4 GiB，lrzsz 无法上传");
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(u64::from(u32::MAX)) as u32);

    let mut sender = Sender::new().context("初始化 ZMODEM 发送端失败")?;
    sender
        .start_file({
            // 普通文件、0644；rz 收到后按 umask 落盘。
            let info = FileInfo::new(remote_name.as_bytes(), Some(Position::new(size as u32)))
                .with_mode(0o100_644);
            match modified {
                Some(modified) => info.with_modified(modified),
                None => info,
            }
        })
        .context("ZMODEM 文件信息不被接受")?;
    // 单文件会话：协商完成后直接排队 ZFIN。
    sender.finish().context("ZMODEM 结束标记排队失败")?;
    let _ = progress.send((0, size));

    let mut pending: Vec<u8> = Vec::new();
    let mut got_wire = false;
    let mut last_rx = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            sender.abort();
            anyhow::bail!("ZMODEM 传输已取消");
        }
        match sender.poll() {
            Action::WriteWire(bytes) => {
                let chunk = bytes.to_vec();
                sender.wire_written(chunk.len());
                uplink
                    .send(chunk)
                    .map_err(|_| anyhow::anyhow!("会话上行通道已关闭"))?;
                continue;
            }
            Action::ReadFile { offset, max_len } => {
                let mut chunk = vec![0u8; max_len.min(64 * 1024)];
                file.seek(std::io::SeekFrom::Start(u64::from(offset.get())))
                    .await
                    .context("定位本地文件失败")?;
                let read = file.read(&mut chunk).await.context("读取本地文件失败")?;
                if read == 0 {
                    sender.abort();
                    anyhow::bail!("本地文件在传输过程中被截断");
                }
                sender
                    .submit_file(&chunk[..read])
                    .context("ZMODEM 数据提交失败")?;
                let _ = progress.send((u64::from(offset.get()) + read as u64, size));
                continue;
            }
            Action::Event(Event::SessionCompleted) => {
                let _ = progress.send((size, size));
                return Ok(transfer::Summary {
                    bytes: size,
                    elapsed: started.elapsed(),
                });
            }
            Action::Event(Event::Aborted) => anyhow::bail!("远端中止了 ZMODEM 传输"),
            Action::Event(_) => continue,
            Action::Idle => {}
            _ => continue,
        }

        if !pending.is_empty() {
            match sender.submit_wire(&pending) {
                Ok(0) => {}
                Ok(consumed) => {
                    pending.drain(..consumed);
                    continue;
                }
                Err(error) => {
                    sender.abort();
                    anyhow::bail!("ZMODEM 协议错误：{error}");
                }
            }
        }

        let waited = if got_wire {
            last_rx.elapsed()
        } else {
            started.elapsed()
        };
        let limit = if got_wire {
            IDLE_TIMEOUT
        } else {
            FIRST_RESPONSE
        };
        if waited >= limit {
            sender.abort();
            if got_wire {
                anyhow::bail!(
                    "ZMODEM 传输超时（远端 {} 秒无应答）",
                    IDLE_TIMEOUT.as_secs()
                );
            }
            anyhow::bail!("远端没有启动 ZMODEM 接收：请确认当前目录的 shell 里已安装 lrzsz（rz）");
        }
        match tokio::time::timeout(TICK.min(limit - waited), downlink.recv()).await {
            Ok(Some(bytes)) => {
                // 只把含 ZDLE 的流量当「协议应答」：远端没有 rz 时，shell 的
                // 回显与 command not found 文本不含 ZDLE，不会再把判定切换到
                // IDLE 路径并不断重置空闲计时——那会形成无限重发的活锁，
                // 终端输出也被整体吞掉、又没有取消入口。FIRST_RESPONSE 到点
                // 即可干净报错「远端没有启动 ZMODEM 接收」。
                if bytes.contains(&ZDLE) {
                    got_wire = true;
                }
                last_rx = Instant::now();
                // 握手完成前只收协议流量（回显文本不进状态机）；拿到真实
                // 应答后照单全收——大块文件数据可能整段不含 ZDLE，不能再滤。
                if got_wire || bytes.contains(&ZDLE) {
                    pending.extend_from_slice(&bytes);
                }
            }
            Ok(None) => anyhow::bail!("会话下行通道已关闭"),
            Err(_) => {
                // 只在等接收端初始化时真正重发；其它阶段是空操作。
                let _ = sender.timeout();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use zmodem2::{Action, Event, Receiver};

    use super::send_file;

    #[tokio::test]
    async fn send_file_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "ruterm-zmodem-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let payload: Vec<u8> = (0..192 * 1024).map(|i| (i * 31 % 251) as u8).collect();
        tokio::fs::write(&path, &payload).await.unwrap();

        let (down_tx, mut down_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (up_tx, mut up_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<(u64, u64)>();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let local = path.clone();
        let sender = tokio::spawn(async move {
            let cancel = &cancel;
            send_file(
                &local,
                "payload.bin",
                &mut down_rx,
                &up_tx,
                &progress_tx,
                cancel,
            )
            .await
        });

        // 同一进程里跑一个接收端，验证发送端的握手、读文件、进度与收尾。
        let mut receiver = Receiver::new().expect("接收端初始化失败");
        let mut received = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut name = String::new();
        let mut session_done = false;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match receiver.poll() {
                    Action::WriteWire(bytes) => {
                        let chunk = bytes.to_vec();
                        receiver.wire_written(chunk.len());
                        assert!(down_tx.send(chunk).is_ok());
                    }
                    Action::WriteFile(bytes) => {
                        let len = bytes.len();
                        received.extend_from_slice(bytes);
                        receiver.file_written(len).expect("接收端写文件失败");
                    }
                    Action::Event(Event::FileStarted(info)) => {
                        name = String::from_utf8_lossy(info.name).into_owned();
                        assert_eq!(info.size.map(|size| size.get()), Some(payload.len() as u32));
                    }
                    Action::Event(Event::FileCompleted) => {}
                    Action::Event(Event::SessionCompleted) => {
                        // 完成事件先于已排队的 ZFIN 字节出队，继续把线缆写完。
                        session_done = true;
                    }
                    Action::Event(Event::Aborted) => panic!("接收端中止了传输"),
                    Action::Idle => {
                        if session_done {
                            break;
                        }
                        if !pending.is_empty() {
                            match receiver.submit_wire(&pending) {
                                Ok(0) => {}
                                Ok(consumed) => {
                                    pending.drain(..consumed);
                                    continue;
                                }
                                Err(error) => panic!("提交 wire 失败：{error}"),
                            }
                        }
                        match tokio::time::timeout(Duration::from_secs(10), up_rx.recv()).await {
                            Ok(Some(bytes)) => pending.extend_from_slice(&bytes),
                            Ok(None) => panic!("发送端上行通道关闭"),
                            Err(_) => receiver.timeout().expect("接收端超时重发失败"),
                        }
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("ZMODEM 会话超时");

        let summary = sender.await.expect("发送任务 panic").expect("发送失败");
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(name, "payload.bin");
        assert_eq!(received, payload);
        assert_eq!(summary.bytes, payload.len() as u64);
        let mut last = None;
        while let Ok(progress) = progress_rx.try_recv() {
            last = Some(progress);
        }
        assert_eq!(last, Some((payload.len() as u64, payload.len() as u64)));
    }

    /// 取消标记预置位时，`send_file` 应立刻收场并报「已取消」，
    /// 不能继续推进协议状态机。
    #[tokio::test]
    async fn cancel_flag_aborts_promptly() {
        let path = std::env::temp_dir().join(format!(
            "ruterm-zmodem-cancel-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::write(&path, b"payload").await.unwrap();

        let (_down_tx, mut down_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (up_tx, _up_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel::<(u64, u64)>();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let local = path.clone();
        let result = send_file(
            &local,
            "payload.bin",
            &mut down_rx,
            &up_tx,
            &progress_tx,
            &cancel,
        )
        .await
        .expect_err("预置取消应直接报错");
        tokio::fs::remove_file(&path).await.ok();
        assert!(result.to_string().contains("已取消"));
    }
}
