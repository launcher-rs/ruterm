//! 会话日志：把远程会话的终端下行字节原样落盘（设置「会话」分页开关，默认关）。
//!
//! 只记**远端 → 终端**方向的字节（含协议横幅）；上行按键绝不落盘——密码输入
//! 可能出现在按键流里，落盘即泄密，这与「密码不入库」同一条红线。
//! 写入走专用线程 + 有界外的 std 通道，句柄全部 drop 后线程冲刷收尾；
//! 一个会话一个文件（重连也算新会话），文件名 `<UTC 时间戳>-<协议>-<地址>.log`。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use rgpui_term::FeedHandle;

/// 数据目录下的日志子目录名。
pub(crate) const LOGS_DIR: &str = "logs";

/// 一个会话的日志句柄。Clone 廉价（内部只是通道发送端）；
/// 所有克隆都 drop 后写线程冲刷并退出。
#[derive(Clone)]
pub(crate) struct SessionLog {
    tx: mpsc::Sender<Vec<u8>>,
}

impl SessionLog {
    /// 打开日志文件并拉起写线程；目录或文件创建失败返回 None（只记日志，
    /// 不影响会话本身）。`stem` 里的非法文件名字符会被替换成 `_`。
    pub(crate) fn open(dir: &Path, stem: &str) -> Option<Self> {
        std::fs::create_dir_all(dir).ok()?;
        let path = unique_log_path(dir, stem)?;
        let file = std::fs::File::create(&path).ok()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("ruterm-session-log".to_string())
            .spawn(move || {
                let mut writer = std::io::BufWriter::new(file);
                // recv 返回 Err = 所有句柄已 drop（会话结束）：冲刷后退出。
                while let Ok(bytes) = rx.recv() {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                }
                let _ = writer.flush();
            })
            .ok()?;
        log::info!("会话日志：{}", path.display());
        Some(Self { tx })
    }

    /// 记录一段下行字节（非阻塞；写线程已退出时静默丢弃）。
    pub(crate) fn record(&self, bytes: &[u8]) {
        let _ = self.tx.send(bytes.to_vec());
    }
}

/// 会话日志落点：`<数据目录>/logs`。目录不存在时由 open 创建。
pub(crate) fn logs_dir(mode: &crate::paths::StorageMode) -> PathBuf {
    mode.dir().join(LOGS_DIR)
}

/// 唯一化日志路径：`<UTC 时间戳（毫秒精度）>-<净化后的 stem>.log`；
/// 同毫秒撞名时追加 `-2`、`-3`… 直到落空（File::create 会截断旧文件，必须避开）。
fn unique_log_path(dir: &Path, stem: &str) -> Option<PathBuf> {
    let stem = sanitize_stem(stem);
    let mut path = dir.join(format!("{}-{stem}.log", utc_stamp()));
    let mut ordinal = 2;
    while path.exists() {
        path = dir.join(format!("{}-{stem}-{ordinal}.log", utc_stamp()));
        ordinal += 1;
    }
    Some(path)
}

/// UTC 时间戳 `YYYYMMDD-HHMMSS-mmm`（文件名可排序；本地时区不引入时间库，注明 UTC）。
fn utc_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.subsec_millis();
    let (year, month, day, hour, minute, second) = civil_from_unix(now.as_secs());
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{millis:03}")
}

/// Unix 秒 → UTC 年月日时分秒（Howard Hinnant 的 civil_from_days 算法，无需时间库）。
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = (rem / 3_600) as u32;
    let minute = (rem % 3_600 / 60) as u32;
    let second = (rem % 60) as u32;
    // 1970-01-01 = 第 0 天，偏移到公元纪元（0000-03-01 = 第 0 天）再反推。
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}

/// 文件名净化：保留字母数字与 `-._@`，其余（`:` `/` `\` 空格等）替换为 `_`。
fn sanitize_stem(stem: &str) -> String {
    let cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // 空 stem 兜底，避免文件名只剩时间戳后面跟一个空段。
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

/// 带可选会话日志的下行注入句柄：`feed` / `end_session` 语义与
/// [`FeedHandle`] 完全一致，字节先进日志再进终端。
///
/// 协议层（Telnet / TCP / WebSocket / 串口 / SSH）一律持有本类型：
/// 站点代码不变，只把字段类型换掉；日志关闭时是纯转发。
#[derive(Clone)]
pub(crate) struct LoggingFeed {
    feed: FeedHandle,
    log: Option<SessionLog>,
}

impl LoggingFeed {
    pub(crate) fn new(feed: FeedHandle, log: Option<SessionLog>) -> Self {
        Self { feed, log }
    }

    /// 注入远端返回的原始字节（先落日志再进终端）。
    pub(crate) fn feed(&self, bytes: impl Into<Vec<u8>>) -> bool {
        let bytes = bytes.into();
        if let Some(log) = &self.log {
            log.record(&bytes);
        }
        self.feed.feed(bytes)
    }

    /// 结束终端会话（透传 [`FeedHandle::end_session`]）。
    pub(crate) fn end_session(&self) -> bool {
        self.feed.end_session()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ruterm-session-log-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 等写线程冲刷：句柄 drop 后轮询文件内容到位。
    fn wait_for_content(path: &Path, expected: &[u8]) -> Vec<u8> {
        for _ in 0..100 {
            if let Ok(bytes) = std::fs::read(path)
                && bytes == expected
            {
                return bytes;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::read(path).unwrap_or_default()
    }

    #[test]
    fn open_record_drop_flushes_to_disk() {
        let dir = temp_dir("roundtrip");
        let log = SessionLog::open(&dir, "ssh-host_22").expect("打开日志");
        log.record(b"hello ");
        log.record("世界\r\n".as_bytes());
        drop(log);
        let files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "只应落一个文件");
        let content = wait_for_content(&files[0], "hello 世界\r\n".as_bytes());
        assert_eq!(content, b"hello \xe4\xb8\x96\xe7\x95\x8c\r\n");
        let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with("-ssh-host_22.log"), "文件名含净化后的 stem：{name}");
        assert!(name.starts_with("20"), "文件名以 UTC 年份开头：{name}");
    }

    #[test]
    fn same_stem_collisions_get_unique_names() {
        let dir = temp_dir("collision");
        let mut paths = Vec::new();
        for _ in 0..3 {
            // 同一毫秒内连开三个：文件名必须互不相同，否则后者截断前者。
            let first = unique_log_path(&dir, "tcp-h").unwrap();
            std::fs::File::create(&first).unwrap();
            paths.push(first);
        }
        let mut names: Vec<String> = paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before, "文件名不得重复");
    }

    #[test]
    fn sanitize_keeps_common_chars_and_replaces_the_rest() {
        assert_eq!(sanitize_stem("ssh-user@host:22"), "ssh-user@host_22");
        assert_eq!(sanitize_stem("serial-/dev/ttyUSB0"), "serial-_dev_ttyUSB0");
        assert_eq!(sanitize_stem("ws-h:1?2*3"), "ws-h_1_2_3");
        assert_eq!(sanitize_stem(""), "session");
    }

    #[test]
    fn civil_from_unix_matches_known_instants() {
        // 2026-11-28 00:00:00 UTC = 1_795_824_000。
        assert_eq!(civil_from_unix(1_795_824_000), (2026, 11, 28, 0, 0, 0));
        // 1970-01-01 00:00:00（纪元起点）。
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2000-02-29（闰年）= 951_782_400。
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29, 0, 0, 0));
    }

    #[test]
    fn logging_feed_writes_through_even_without_a_log() {
        // 日志关闭时 LoggingFeed 是纯转发：feed 仍要进终端（解析线程活着，返回 true）。
        let mut builder = rgpui_term::TerminalBuilder::new_external(None).expect("外联后端");
        let rgpui_term::ExternalHandles { feed, .. } =
            builder.take_external_handles().expect("外联句柄");
        let logging = LoggingFeed::new(feed, None);
        assert!(logging.feed(b"hi"));
        assert!(logging.end_session());
    }
}
