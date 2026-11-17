//! SFTP 文件传输：复用当前 SSH 连接的 SFTP 子系统做上传 / 下载。
//!
//! 传输任务由会话任务在收到 [`Task`] 后拉起，文件内容分块搬运并按节流节奏
//! 上报 `(已传输, 总量)`；本地路径与远端路径都只以参数形态出现，不拼 shell 命令。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use russh_sftp::client::{Config as SftpConfig, SftpSession, error::Error as SftpError};
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
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
    let home = if remote == "~" || remote.starts_with("~/") {
        remote_home(sftp).await?
    } else {
        String::new()
    };
    expand_remote_path(remote, &home)
}

fn expand_remote_path(remote: &str, home: &str) -> Result<String> {
    // POSIX 文件名可以包含首尾空白；这里只拒绝真正的空串。
    if remote.is_empty() {
        anyhow::bail!("远端路径不能为空");
    }
    if remote == "~" {
        return Ok(home.to_owned());
    }
    if let Some(rest) = remote.strip_prefix("~/") {
        return Ok(format!("{}/{rest}", home.trim_end_matches('/')));
    }
    Ok(remote.to_owned())
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
    let result = async {
        let mut remote = resolve_remote(&sftp, &remote).await?;
        // 先 lstat，不能把目标目录的 symlink 悄悄跟随成另一个路径。
        let mut local = local;
        if direction == Direction::Download
            && local_target_metadata(&local)
                .await?
                .is_some_and(|m| m.is_dir())
            && let Some(name) = Path::new(&remote).file_name()
        {
            local.push(name);
        }
        if direction == Direction::Upload {
            let metadata = remote_target_metadata(&sftp, &remote).await?;
            if remote.ends_with('/') || metadata.is_some_and(|m| m.is_dir()) {
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
        Ok(Summary {
            bytes,
            elapsed: started.elapsed(),
        })
    }
    .await;
    // 校验或传输失败也要关闭会话；不能因 close 失败谎报已完成的提交失败。
    let _ = sftp.close().await;
    result.map_err(|error| transfer_failure(error, 0))
}

async fn upload(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    progress: &UnboundedSender<(u64, u64)>,
) -> Result<u64> {
    // 只检查目标类型：普通文件允许覆盖，目录 / 符号链接 / 特殊文件拒绝。
    if let Some(existing) = remote_target_metadata(sftp, remote).await? {
        if existing.is_dir() {
            anyhow::bail!("远端目标是目录：{remote}");
        }
        if !existing.is_regular() && existing.permissions.is_some() {
            anyhow::bail!("远端目标不是普通文件：{remote}");
        }
    }
    let mut source = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("无法读取本地文件 {}", local.display()))?;
    let metadata = source.metadata().await.context("无法读取本地文件信息")?;
    if !metadata.is_file() {
        anyhow::bail!("本地源不是普通文件：{}", local.display());
    }
    let total = metadata.len();
    let temp = remote_sibling(remote, "tmp")?;
    // EXCLUDE 是本库对 SSH_FXF_EXCL 的命名。创建失败时不能删除该路径，
    // 它可能属于别人；仅在成功取得独占句柄后才承担清理责任。
    let mut file = sftp
        .open_with_flags_and_attributes(
            temp.as_str(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
            FileAttributes {
                permissions: Some(0o600),
                ..FileAttributes::empty()
            },
        )
        .await
        .with_context(|| format!("无法独占创建远端暂存文件 {temp}"))?;
    let mut done = 0;
    let result = async {
        done = copy_with_progress(&mut source, &mut file, total, progress).await?;
        file.flush().await.context("远端写入收尾失败")?;
        check_length(source.metadata().await?.len(), total)?;
        // 新文件沿用源的普通权限位，不从本地 Windows 元数据凭空引入可执行位。
        #[cfg(unix)]
        let permissions = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o777
        };
        #[cfg(not(unix))]
        let permissions = if metadata.permissions().readonly() {
            0o444
        } else {
            0o644
        };
        file.set_metadata(FileAttributes {
            permissions: Some(permissions),
            ..FileAttributes::empty()
        })
        .await
        .context("设置远端文件权限失败")?;
        file.shutdown().await.context("远端文件关闭失败")?;
        commit_remote_temp(sftp, &temp, remote).await?;
        Ok(done)
    }
    .await;
    if result.is_err() {
        let _ = file.shutdown().await;
    }
    drop(file);
    match result {
        Ok(bytes) => Ok(bytes),
        Err(error) => {
            let error = transfer_failure(error, done);
            match sftp.remove_file(temp.as_str()).await {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    Err(error.context(format!("清理远端暂存文件 {temp} 失败：{cleanup}")))
                }
            }
        }
    }
}

async fn download(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    progress: &UnboundedSender<(u64, u64)>,
) -> Result<u64> {
    let mut source = sftp
        .open(remote)
        .await
        .with_context(|| format!("无法打开远端文件 {remote}"))?;
    let result = async {
        // 以已打开句柄的大小为准，避免 stat/open 之间换源。
        let metadata = source.metadata().await.context("无法读取远端源文件信息")?;
        if metadata.is_dir() {
            anyhow::bail!("远端路径是目录：{remote}");
        }
        let total = metadata
            .size
            .context("远端未返回源文件长度，无法安全校验")?;
        let (temp, done) = copy_to_local_temp(&mut source, local, total, progress).await?;
        let result = async {
            let size = source
                .metadata()
                .await?
                .size
                .context("远端未返回源文件长度")?;
            check_length(size, total)?;
            source.shutdown().await.context("远端源文件关闭失败")?;
            commit_local_temp(&temp, local, metadata.permissions).await?;
            Ok(done)
        }
        .await;
        match result {
            Ok(bytes) => Ok(bytes),
            Err(error) => Err(cleanup_local_temp(&temp, transfer_failure(error, done)).await),
        }
    }
    .await;
    let _ = source.shutdown().await;
    result.with_context(|| format!("下载 {remote} 失败"))
}

async fn local_target_metadata(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("拒绝写入符号链接目标：{}", path.display())
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("无法检查本地目标 {}", path.display())),
    }
}

async fn remote_target_metadata(sftp: &SftpSession, path: &str) -> Result<Option<FileAttributes>> {
    // 去掉目录提示用的斜杠再 lstat，否则 symlink/ 会被服务器跟随。
    let probe = path.trim_end_matches('/');
    let probe = if probe.is_empty() { "/" } else { probe };
    match sftp.symlink_metadata(probe).await {
        Ok(metadata) if metadata.is_symlink() => anyhow::bail!("拒绝写入远端符号链接目标：{path}"),
        Ok(metadata) => Ok(Some(metadata)),
        Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(None),
        Err(error) => Err(error).with_context(|| format!("无法检查远端目标 {path}")),
    }
}

/// 用「旧文件先改名备份 → 暂存改名到位 → 删备份」提交覆盖。
///
/// 普通 SSH_FXP_RENAME 在目标已存在时会失败（OpenSSH 用 link+unlink 实现），
/// 而本库没有暴露 posix-rename 扩展。绝不截断目标原地写入；任何一步失败都
/// 尽量把旧文件恢复原位，恢复不了也会在错误里给出备份路径。
async fn commit_remote_temp(sftp: &SftpSession, temp: &str, remote: &str) -> Result<()> {
    let Some(existing) = remote_target_metadata(sftp, remote).await? else {
        return sftp.rename(temp, remote).await.context("远端提交失败");
    };
    if existing.is_dir() {
        anyhow::bail!("远端目标是目录：{remote}");
    }
    if !existing.is_regular() && existing.permissions.is_some() {
        anyhow::bail!("远端目标不是普通文件：{remote}");
    }
    let backup = remote_sibling(remote, "bak")?;
    sftp.rename(remote, backup.as_str())
        .await
        .context("无法暂存已有远端文件")?;
    match sftp.rename(temp, remote).await {
        Ok(()) => {
            if let Err(error) = sftp.remove_file(backup.as_str()).await {
                log::warn!("远端旧文件备份清理失败（{backup}）：{error}");
            }
            Ok(())
        }
        Err(error) => match sftp.rename(backup.as_str(), remote).await {
            Ok(()) => Err(error).context("远端提交失败，原文件已恢复"),
            Err(restore) => Err(error).context(format!(
                "远端提交失败，且原文件恢复失败；原文件保存在 {backup}：{restore}"
            )),
        },
    }
}

fn temp_name() -> Result<String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).context("生成传输暂存文件随机名失败")?;
    Ok(format!(
        ".ruterm-transfer-{:032x}.tmp",
        u128::from_be_bytes(random)
    ))
}

/// 在远端目标同目录生成随机文件名；不使用本机 `Path`，因为 Windows 的
/// 反斜杠在远端 POSIX 文件名里只是普通字符。
fn remote_sibling(remote: &str, extension: &str) -> Result<String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).context("生成传输暂存文件随机名失败")?;
    let parent = remote.rfind('/').map_or("", |index| &remote[..=index]);
    Ok(format!(
        "{parent}.ruterm-transfer-{:032x}.{extension}",
        u128::from_be_bytes(random)
    ))
}

/// 只生成已关闭且校验完成的暂存文件；调用方检查源句柄后才可提交。
async fn copy_to_local_temp<R: AsyncRead + Unpin>(
    source: &mut R,
    local: &Path,
    total: u64,
    progress: &UnboundedSender<(u64, u64)>,
) -> Result<(PathBuf, u64)> {
    if local_target_metadata(local)
        .await?
        .is_some_and(|m| !m.is_file())
    {
        anyhow::bail!("本地目标不是普通文件：{}", local.display());
    }
    let temp = local.with_file_name(temp_name()?);
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut out = options
        .open(&temp)
        .await
        .with_context(|| format!("无法独占创建本地暂存文件 {}", temp.display()))?;
    let mut done = 0;
    let result = async {
        done = copy_with_progress(source, &mut out, total, progress).await?;
        out.flush().await.context("本地写入收尾失败")?;
        out.sync_all().await.context("同步本地暂存文件失败")?;
        out.shutdown().await.context("本地暂存文件关闭失败")?;
        Ok(())
    }
    .await;
    // Windows 必须释放句柄后才能重命名或清理；flush/sync_all 已检查延迟写错误。
    drop(out);
    match result {
        Ok(()) => Ok((temp, done)),
        Err(error) => Err(cleanup_local_temp(&temp, transfer_failure(error, done)).await),
    }
}

async fn commit_local_temp(temp: &Path, local: &Path, source_mode: Option<u32>) -> Result<()> {
    // 提交前再检查一次，保留已有目标的权限，而不是套用新文件默认值。
    let existing = local_target_metadata(local).await?;
    if let Some(metadata) = existing {
        if !metadata.is_file() {
            anyhow::bail!("本地目标不是普通文件：{}", local.display());
        }
        tokio::fs::set_permissions(temp, metadata.permissions())
            .await
            .context("保留本地目标权限失败")?;
    } else {
        #[cfg(unix)]
        if let Some(mode) = source_mode {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(temp, std::fs::Permissions::from_mode(mode & 0o777))
                .await
                .context("设置下载文件权限失败")?;
        }
        #[cfg(not(unix))]
        let _ = source_mode;
    }
    tokio::fs::rename(temp, local)
        .await
        .with_context(|| format!("原子提交本地文件 {} 失败（原目标未删除）", local.display()))
}

async fn cleanup_local_temp(temp: &Path, error: anyhow::Error) -> anyhow::Error {
    #[cfg(windows)]
    if let Ok(metadata) = tokio::fs::symlink_metadata(temp).await
        && metadata.is_file()
        && !metadata.file_type().is_symlink()
        && metadata.permissions().readonly()
    {
        // 仅撤销本次暂存文件的只读位；绝不改最终目标来强行覆盖。
        let mut permissions = metadata.permissions();
        // 故意清除只读位：暂存文件只读时无法删除，需先恢复可写再清理。
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = tokio::fs::set_permissions(temp, permissions).await;
    }
    match tokio::fs::remove_file(temp).await {
        Ok(()) => error,
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => error,
        Err(cleanup) => error.context(format!("清理暂存文件 {} 失败：{cleanup}", temp.display())),
    }
}

fn check_length(actual: u64, expected: u64) -> Result<()> {
    if actual != expected {
        anyhow::bail!("源文件长度不一致：预期 {expected} 字节，实际 {actual} 字节");
    }
    Ok(())
}

/// 保持 Task/execute 的 Result 接口；失败也携带实际已被 writer 接受的字节数。
/// SFTP 流水线写入尚未确认时，这不是远端持久化字节数。
#[derive(Debug)]
struct TransferFailure {
    bytes: u64,
    cause: anyhow::Error,
}

impl std::fmt::Display for TransferFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "已传输 {} 字节：{:#}", self.bytes, self.cause)
    }
}

impl std::error::Error for TransferFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

fn transfer_failure(error: anyhow::Error, bytes: u64) -> anyhow::Error {
    if error.downcast_ref::<TransferFailure>().is_some() {
        error
    } else {
        TransferFailure {
            bytes,
            cause: error,
        }
        .into()
    }
}

/// 分块搬运，必须匹配长度并探测 EOF；成功/失败均补发真实终值。
async fn copy_with_progress<R, W>(
    reader: &mut R,
    writer: &mut W,
    total: u64,
    progress: &UnboundedSender<(u64, u64)>,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; CHUNK];
    let mut done = 0u64;
    let mut last_report = Instant::now();
    let _ = progress.send((0, total));
    let result = async {
        loop {
            // 到达预期长度后仍读一个字节，增长的源不能被悄悄截成预期大小。
            let limit = (total - done).min(CHUNK as u64).max(1) as usize;
            let read = reader
                .read(&mut buffer[..limit])
                .await
                .context("读取数据失败")?;
            if read == 0 {
                check_length(done, total)?;
                return Ok(done);
            }
            if done == total {
                anyhow::bail!("源文件长度不一致：源已增长，超过预期 {total} 字节");
            }
            let mut offset = 0;
            while offset < read {
                // 不能用 write_all：错误前的部分写入也需要准确计数。
                let written = writer
                    .write(&buffer[offset..read])
                    .await
                    .map_err(write_error)?;
                if written == 0 {
                    return Err(write_error(std::io::ErrorKind::WriteZero.into()));
                }
                offset += written;
                done += written as u64;
                if last_report.elapsed() >= PROGRESS_INTERVAL {
                    let _ = progress.send((done, total));
                    last_report = Instant::now();
                }
            }
        }
    }
    .await;
    let _ = progress.send((done, total));
    result.map_err(|error| transfer_failure(error, done))
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
    use super::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    struct Reader {
        data: &'static [u8],
        fail_at_end: bool,
        pending: bool,
    }

    impl Reader {
        fn new(data: &'static [u8], fail_at_end: bool) -> Self {
            Self {
                data,
                fail_at_end,
                pending: true,
            }
        }
    }

    impl AsyncRead for Reader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.pending {
                self.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.pending = true;
            if self.data.is_empty() && self.fail_at_end {
                return Poll::Ready(Err(io::Error::other("injected read failure")));
            }
            let count = self.data.len().min(buf.remaining()).min(2);
            buf.put_slice(&self.data[..count]);
            self.data = &self.data[count..];
            Poll::Ready(Ok(()))
        }
    }

    struct Writer {
        data: Vec<u8>,
        limit: usize,
        zero_at_limit: bool,
        pending: bool,
    }

    impl Writer {
        fn new(limit: usize, zero_at_limit: bool) -> Self {
            Self {
                data: Vec::new(),
                limit,
                zero_at_limit,
                pending: true,
            }
        }
    }

    impl AsyncWrite for Writer {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.pending {
                self.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.pending = true;
            let count = buf.len().min(2).min(self.limit - self.data.len());
            if count == 0 && !self.zero_at_limit {
                return Poll::Ready(Err(io::Error::other("injected write failure")));
            }
            self.data.extend_from_slice(&buf[..count]);
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn last_progress(rx: &mut UnboundedReceiver<(u64, u64)>) -> (u64, u64) {
        let mut last = None;
        while let Ok(value) = rx.try_recv() {
            last = Some(value);
        }
        last.expect("copy must report progress even on failure")
    }

    fn failure_bytes(error: &anyhow::Error) -> u64 {
        error
            .downcast_ref::<TransferFailure>()
            .expect("actual byte count in error")
            .bytes
    }

    struct TestDir(PathBuf);

    impl TestDir {
        async fn new() -> Self {
            let path = std::env::temp_dir().join(temp_name().unwrap());
            tokio::fs::create_dir(&path).await.unwrap();
            Self(path)
        }

        async fn assert_only_target(&self) {
            let mut entries = tokio::fs::read_dir(&self.0).await.unwrap();
            assert_eq!(
                entries.next_entry().await.unwrap().unwrap().file_name(),
                "target"
            );
            assert!(
                entries.next_entry().await.unwrap().is_none(),
                "temporary file leaked"
            );
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn remote_paths_preserve_whitespace_and_tilde_suffix() {
        for path in [" file ", " \t\n", "/dir/file ", "~user/ file ", " ~", "~ "] {
            assert_eq!(expand_remote_path(path, "/home/user").unwrap(), path);
        }
        assert!(expand_remote_path("", "/home/user").is_err());
        assert_eq!(
            expand_remote_path("~", "/home/ user ").unwrap(),
            "/home/ user "
        );
        assert_eq!(
            expand_remote_path("~/ file \t", "/home/ user /").unwrap(),
            "/home/ user / file \t"
        );
        assert_eq!(
            expand_remote_path("~// file ", "/home/user").unwrap(),
            "/home/user// file "
        );
        assert_eq!(expand_remote_path("~/ file ", "/").unwrap(), "/ file ");
    }

    #[test]
    fn remote_temp_paths_stay_in_posix_parent() {
        for (path, parent) in [
            ("file", ""),
            ("/ file ", "/"),
            (" dir /file ", " dir /"),
            ("dir/a\\b", "dir/"),
        ] {
            let temp = remote_sibling(path, "tmp").unwrap();
            assert!(temp.starts_with(&format!("{parent}.ruterm-transfer-")));
            assert!(temp.ends_with(".tmp"));
            assert!(!temp[parent.len()..].contains('/'));
            assert_ne!(temp, remote_sibling(path, "tmp").unwrap());
        }
    }

    #[tokio::test]
    async fn copy_returns_actual_bytes_after_exact_eof() {
        let (tx, mut rx) = unbounded_channel();
        let mut reader = Reader::new(b"abcdef", false);
        let mut writer = Writer::new(usize::MAX, false);
        assert_eq!(
            copy_with_progress(&mut reader, &mut writer, 6, &tx)
                .await
                .unwrap(),
            6
        );
        assert_eq!(writer.data, b"abcdef");
        assert_eq!(last_progress(&mut rx), (6, 6));
        assert_eq!(
            copy_with_progress(&mut &b""[..], &mut writer, 0, &tx)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn copy_rejects_truncated_source() {
        let (tx, mut rx) = unbounded_channel();
        let mut reader = Reader::new(b"abc", false);
        let mut writer = Writer::new(usize::MAX, false);
        let error = copy_with_progress(&mut reader, &mut writer, 6, &tx)
            .await
            .unwrap_err();
        assert_eq!(failure_bytes(&error), 3);
        assert!(error.to_string().contains("长度不一致"));
        assert_eq!(writer.data, b"abc");
        assert_eq!(last_progress(&mut rx), (3, 6));
    }

    #[tokio::test]
    async fn copy_rejects_growing_source() {
        let (tx, mut rx) = unbounded_channel();
        let mut reader = Reader::new(b"abcdef", false);
        let mut writer = Writer::new(usize::MAX, false);
        let error = copy_with_progress(&mut reader, &mut writer, 3, &tx)
            .await
            .unwrap_err();
        assert_eq!(failure_bytes(&error), 3);
        assert!(error.to_string().contains("源已增长"));
        assert_eq!(writer.data, b"abc");
        assert_eq!(last_progress(&mut rx), (3, 3));
        let error = copy_with_progress(&mut reader, &mut writer, 0, &tx)
            .await
            .unwrap_err();
        assert_eq!(failure_bytes(&error), 0);
    }

    #[tokio::test]
    async fn copy_reports_read_failure_bytes_even_at_expected_length() {
        let (tx, mut rx) = unbounded_channel();
        let mut reader = Reader::new(b"abc", true);
        let mut writer = Writer::new(usize::MAX, false);
        let error = copy_with_progress(&mut reader, &mut writer, 3, &tx)
            .await
            .unwrap_err();
        assert_eq!(failure_bytes(&error), 3);
        assert!(error.to_string().contains("injected read failure"));
        assert_eq!(last_progress(&mut rx), (3, 3));
    }

    #[tokio::test]
    async fn copy_reports_partial_write_failure_bytes() {
        let (tx, mut rx) = unbounded_channel();
        let mut writer = Writer::new(3, false);
        let error = copy_with_progress(&mut &b"abcdef"[..], &mut writer, 6, &tx)
            .await
            .unwrap_err();
        assert_eq!(failure_bytes(&error), 3);
        assert!(error.to_string().contains("injected write failure"));
        assert_eq!(writer.data, b"abc");
        assert_eq!(last_progress(&mut rx), (3, 6));
        // 外层的收尾/清理错误不能把内部已经计数的部分写入重置成零。
        assert_eq!(
            failure_bytes(&transfer_failure(error.context("outer"), 0)),
            3
        );
    }

    #[tokio::test]
    async fn copy_rejects_write_zero_without_losing_progress() {
        let (tx, mut rx) = unbounded_channel();
        let mut writer = Writer::new(1, true);
        let error = copy_with_progress(&mut &b"abc"[..], &mut writer, 3, &tx)
            .await
            .unwrap_err();
        assert_eq!(failure_bytes(&error), 1);
        assert_eq!(last_progress(&mut rx), (1, 3));
    }

    #[tokio::test]
    async fn local_staging_failures_preserve_original_and_remove_only_temp() {
        let dir = TestDir::new().await;
        let target = dir.0.join("target");
        tokio::fs::write(&target, b"original").await.unwrap();
        let (tx, _) = unbounded_channel();
        for (data, fail, total, done) in [
            (&b"abc"[..], false, 6, 3),
            (&b"abcdef"[..], false, 3, 3),
            (&b"abc"[..], true, 6, 3),
        ] {
            let mut reader = Reader::new(data, fail);
            let error = copy_to_local_temp(&mut reader, &target, total, &tx)
                .await
                .unwrap_err();
            assert_eq!(failure_bytes(&error), done);
            assert_eq!(tokio::fs::read(&target).await.unwrap(), b"original");
            dir.assert_only_target().await;
        }
    }

    #[tokio::test]
    async fn local_commit_replaces_only_after_validated_staging() {
        let dir = TestDir::new().await;
        let target = dir.0.join("target");
        tokio::fs::write(&target, b"original").await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o751))
                .await
                .unwrap();
        }
        let (tx, _) = unbounded_channel();
        let (temp, bytes) = copy_to_local_temp(&mut &b"replacement"[..], &target, 11, &tx)
            .await
            .unwrap();
        assert_eq!(bytes, 11);
        assert_eq!(temp.parent(), target.parent());
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"original");
        commit_local_temp(&temp, &target, Some(0o600))
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"replacement");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                tokio::fs::metadata(&target)
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o751
            );
        }
        dir.assert_only_target().await;
    }

    #[tokio::test]
    async fn failed_local_commit_preserves_target_directory() {
        let dir = TestDir::new().await;
        let target = dir.0.join("target");
        let (tx, _) = unbounded_channel();
        let (temp, bytes) = copy_to_local_temp(&mut &b"new"[..], &target, 3, &tx)
            .await
            .unwrap();
        tokio::fs::create_dir(&target).await.unwrap();
        tokio::fs::write(target.join("sentinel"), b"original")
            .await
            .unwrap();
        let error = commit_local_temp(&temp, &target, None).await.unwrap_err();
        let error = cleanup_local_temp(&temp, transfer_failure(error, bytes)).await;
        assert_eq!(failure_bytes(&error), 3);
        assert_eq!(
            tokio::fs::read(target.join("sentinel")).await.unwrap(),
            b"original"
        );
        dir.assert_only_target().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_symlink_targets_are_rejected_before_copy_and_commit() {
        let dir = TestDir::new().await;
        let target = dir.0.join("target");
        let original = dir.0.join("original");
        tokio::fs::write(&original, b"original").await.unwrap();
        std::os::unix::fs::symlink(&original, &target).unwrap();
        let (tx, _) = unbounded_channel();
        assert!(
            copy_to_local_temp(&mut &b"new"[..], &target, 3, &tx)
                .await
                .is_err()
        );
        tokio::fs::remove_file(&target).await.unwrap();
        let (temp, _) = copy_to_local_temp(&mut &b"new"[..], &target, 3, &tx)
            .await
            .unwrap();
        std::os::unix::fs::symlink(&original, &target).unwrap();
        let error = commit_local_temp(&temp, &target, None).await.unwrap_err();
        let _ = cleanup_local_temp(&temp, error).await;
        assert_eq!(tokio::fs::read(&original).await.unwrap(), b"original");
        assert!(
            tokio::fs::symlink_metadata(&target)
                .await
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!temp.exists());
        // 悬空链接也必须拒绝，不能当成不存在。
        tokio::fs::remove_file(&original).await.unwrap();
        assert!(local_target_metadata(&target).await.is_err());
    }

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
