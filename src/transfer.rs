//! SFTP 文件传输：复用当前 SSH 连接的 SFTP 子系统做上传 / 下载。
//!
//! 传输任务由会话任务在收到 [`Task`] 后拉起，文件内容分块搬运并按节流节奏
//! 上报 `(已传输, 总量)`；本地路径与远端路径都只以参数形态出现，不拼 shell 命令。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use russh_sftp::client::{Config as SftpConfig, SftpSession};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

/// 单次读写的块大小：太小系统调用多，太大占内存且进度刷新变粗。
const CHUNK: usize = 128 * 1024;
/// 进度上报间隔：大文件按块发会让 UI 频繁重绘，节流到约 10 次/秒。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// 单次 SFTP 请求（写一个包 / 读一次）等服务器回应的上限。
///
/// 库默认只有 10 秒：慢链路上大文件传到一半会突然报 `写入数据失败：Timeout`，
/// 而服务器其实只是还没确认（磁盘或带宽慢）。放宽后由 TCP 自己兜底断链。
const REQUEST_TIMEOUT_SECS: u64 = 120;

/// SFTP 客户端调优：提高并发写请求数与单包大小，提升高延迟链路吞吐。
///
/// 在途请求数别超过 SSH 通道窗口（OpenSSH 默认 2 MiB）：多出来的请求既发不出去，
/// 又会从入队那刻起累计"单请求超时"，慢链路上反而更容易被判超时。
/// 32 × 64 KiB = 2 MiB，正好吃满窗口。
pub fn client_config() -> SftpConfig {
    SftpConfig {
        max_concurrent_writes: 32,
        max_concurrent_reads: 8,
        max_write_packet_len: 64 * 1024,
        request_timeout_secs: REQUEST_TIMEOUT_SECS,
        ..SftpConfig::default()
    }
}

/// 传输方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// 本地 → 远端。
    Upload,
    /// 远端 → 本地。
    Download,
}

impl Direction {
    /// 状态文案用的动词。
    pub fn label(self) -> &'static str {
        match self {
            Self::Upload => "上传",
            Self::Download => "下载",
        }
    }
}

/// 一次传输的完成信息。
pub struct Summary {
    pub bytes: u64,
    pub elapsed: Duration,
}

/// 交给会话任务执行的一次传输任务（进度与结果各自回传）。
pub struct Task {
    pub direction: Direction,
    pub local: PathBuf,
    pub remote: String,
    pub progress: UnboundedSender<(u64, u64)>,
    pub reply: oneshot::Sender<Result<Summary>>,
}

/// 读取远端主目录（SFTP 的 `.` 即登录后的初始目录）。
pub async fn remote_home(sftp: &SftpSession) -> Result<String> {
    sftp.canonicalize(".")
        .await
        .map_err(|error| anyhow::anyhow!("无法读取远端主目录：{error}"))
}

/// 展开远端路径里的 `~`：SFTP 协议不会替客户端做这件事。
pub async fn resolve_remote(sftp: &SftpSession, remote: &str) -> Result<String> {
    let remote = remote.trim();
    if remote.is_empty() {
        anyhow::bail!("远端路径不能为空");
    }
    if remote == "~" {
        return remote_home(sftp).await;
    }
    if let Some(rest) = remote.strip_prefix("~/") {
        let home = remote_home(sftp).await?;
        return Ok(format!("{}/{rest}", home.trim_end_matches('/')));
    }
    Ok(remote.to_string())
}

/// 执行传输：校验路径 → 分块搬运 → 关闭 SFTP 会话。
pub async fn execute(
    sftp: SftpSession,
    direction: Direction,
    local: PathBuf,
    remote: String,
    progress: UnboundedSender<(u64, u64)>,
) -> Result<Summary> {
    let started = Instant::now();
    let remote = resolve_remote(&sftp, &remote).await?;
    // 本地选到目录时按远端文件名落进去，省得用户再补一层文件名。
    let mut local = local;
    if direction == Direction::Download
        && local.is_dir()
        && let Some(name) = Path::new(&remote).file_name()
    {
        local.push(name);
    }
    // 上传目标是目录（如主目录、以 `/` 结尾）时沿用本地文件名，避免把文件
    // 写到目录路径上直接失败。
    let mut remote = remote;
    if direction == Direction::Upload {
        let target_is_dir = remote.ends_with('/')
            || sftp
                .metadata(remote.as_str())
                .await
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false);
        if target_is_dir {
            let name = local
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| anyhow::anyhow!("远端目标是目录，但本地路径没有文件名"))?;
            remote = format!("{}/{}", remote.trim_end_matches('/'), name);
        }
    }
    let bytes = match direction {
        Direction::Upload => upload(&sftp, &local, &remote, &progress).await?,
        Direction::Download => download(&sftp, &local, &remote, &progress).await?,
    };
    let _ = sftp.close().await;
    Ok(Summary {
        bytes,
        elapsed: started.elapsed(),
    })
}

async fn upload(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    progress: &UnboundedSender<(u64, u64)>,
) -> Result<u64> {
    let mut source = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("无法读取本地文件 {}", local.display()))?;
    let metadata = source
        .metadata()
        .await
        .with_context(|| format!("无法读取本地文件信息 {}", local.display()))?;
    if metadata.is_dir() {
        anyhow::bail!("本地路径是目录：{}", local.display());
    }
    let total = metadata.len();
    let mut file = sftp
        .create(remote)
        .await
        .with_context(|| format!("无法在远端创建 {remote}（目录不存在或没有写权限）"))?;
    copy_with_progress(&mut source, &mut file, total, progress)
        .await
        .with_context(|| format!("上传到 {remote} 失败"))?;
    file.flush().await.context("远端写入收尾失败")?;
    file.shutdown().await.context("远端文件关闭失败")?;
    Ok(total)
}

async fn download(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    progress: &UnboundedSender<(u64, u64)>,
) -> Result<u64> {
    let metadata = sftp
        .metadata(remote)
        .await
        .with_context(|| format!("远端文件不可访问：{remote}"))?;
    if metadata.is_dir() {
        anyhow::bail!("远端路径是目录：{remote}");
    }
    let total = metadata.len();
    let mut source = sftp
        .open(remote)
        .await
        .with_context(|| format!("无法打开远端文件 {remote}"))?;
    let mut out = tokio::fs::File::create(local)
        .await
        .with_context(|| format!("无法写入本地文件 {}", local.display()))?;
    if let Err(error) = copy_with_progress(&mut source, &mut out, total, progress).await {
        // 下载半途失败时别把残缺文件留在本地，避免用户误以为传完了。
        let _ = tokio::fs::remove_file(local).await;
        return Err(error.context(format!("下载 {remote} 失败")));
    }
    out.flush().await.context("本地写入收尾失败")?;
    Ok(total)
}

/// 分块搬运并节流上报进度；结束时补发一次终值。
async fn copy_with_progress<R, W>(
    reader: &mut R,
    writer: &mut W,
    total: u64,
    progress: &UnboundedSender<(u64, u64)>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; CHUNK];
    let mut done = 0u64;
    let mut last_report = Instant::now();
    let _ = progress.send((0, total));
    loop {
        let read = reader.read(&mut buffer).await.context("读取数据失败")?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .await
            .map_err(write_error)?;
        done += read as u64;
        if last_report.elapsed() >= PROGRESS_INTERVAL {
            let _ = progress.send((done, total));
            last_report = Instant::now();
        }
    }
    let _ = progress.send((done, total));
    Ok(())
}

/// 写入失败时的统一文案：超时要能看出是"服务器迟迟没确认"而非本地写错。
fn write_error(error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::TimedOut {
        anyhow::anyhow!(
            "写入数据失败：服务器 {REQUEST_TIMEOUT_SECS} 秒未确认（网络或远端磁盘过慢）"
        )
    } else {
        anyhow::anyhow!("写入数据失败：{error}")
    }
}

/// 字节数的紧凑展示（B / KiB / MiB / GiB）。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::format_bytes;

    #[test]
    fn format_bytes_uses_readable_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
