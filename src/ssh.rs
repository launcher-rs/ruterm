//! SSH 引擎：russh 客户端事件处理与主机指纹校验（TOFU，记录存于 ~/.rulink）。
//!
//! 本模块只认识"字节"，不认识终端 UI：下行字节全部灌入
//! [`rgpui_term::FeedHandle`]，会话终止信号（EOF/关闭/退出码）折算成
//! `end_session`，由终端后端统一收尾。

use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rgpui_term::FeedHandle;
use russh::ChannelId;
use russh::client::{self, Session};
use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};

/// 终端侧应当忽略的下行通道集合。
///
/// exec 探测等非交互通道与 PTY 共用同一条 SSH 连接，russh 会把它们的字节
/// 同时投递给 `Channel` 接收端和 [`client::Handler`]；不加甄别就会把探测输出
/// 灌进终端、并把探测通道的 close/exit-status 误判为会话结束。
/// 通道号在连接生命周期内不复用，故只登记、不摘除（摘除反而可能与
/// `channel_close` 回调竞态，导致误结束会话）。
#[derive(Clone, Default)]
pub struct SideChannels(Arc<Mutex<HashSet<ChannelId>>>);

impl SideChannels {
    /// 登记一个非终端通道。
    pub fn add(&self, channel: ChannelId) {
        self.lock().insert(channel);
    }

    /// 判断通道是否属于非终端通道。
    pub fn contains(&self, channel: ChannelId) -> bool {
        self.lock().contains(&channel)
    }

    /// 锁中毒不影响正确性（集合只增不改），降级为内部数据继续用。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<ChannelId>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// RuLink 专用 known_hosts 路径（与 OpenSSH 分开，不污染系统文件）。
pub fn known_hosts_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".rulink").join("known_hosts")
}

/// 主机密钥的 SHA-256 指纹（hex 形式，避免依赖 base64 feature）。
fn fingerprint_hex(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256)
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// russh 会话的事件处理器：实现 `client::Handler`。
pub struct SshTerminalHandler {
    feed: FeedHandle,
    host: String,
    port: u16,
    /// 非终端通道（exec 探测等），其字节与控制消息都不进终端。
    side: SideChannels,
}

impl SshTerminalHandler {
    /// 以下行注入句柄、主机信息与旁路通道集合构造处理器。
    pub fn new(feed: FeedHandle, host: String, port: u16, side: SideChannels) -> Self {
        Self {
            feed,
            host,
            port,
            side,
        }
    }

    /// 首次信任并持久化（Trust On First Use）：
    /// known_hosts 命中则校验，缺失则写入新记录并信任，不符则拒绝。
    fn trust_key(&mut self, key: &PublicKey) -> bool {
        let entry_host = format!("[{}]:{}", self.host, self.port);
        let fp = fingerprint_hex(key);
        let path = known_hosts_path();

        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut has_entry_for_host = false;
        for line in existing.lines() {
            let mut fields = line.split_whitespace();
            let (Some(recorded_host), Some(recorded_fp)) = (fields.next(), fields.next()) else {
                continue;
            };
            if recorded_host == entry_host {
                has_entry_for_host = true;
                if recorded_fp == fp {
                    return true;
                }
            }
        }

        if has_entry_for_host {
            eprintln!(
                "[rulink] 警告: {} 的主机密钥指纹与 known_hosts 不符，拒绝连接（可能是中间人攻击）",
                entry_host
            );
            return false;
        }

        // 首次接触该主机：写入记录并信任。
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            eprintln!(
                "[rulink] 无法创建 {} 目录（{error}），拒绝连接",
                parent.display()
            );
            return false;
        }
        let entry = format!("{entry_host} {fp} {}\n", key.algorithm());
        let mut file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) => {
                // 写盘失败不能降级为"不校验"，直接拒绝更安全。
                eprintln!("[rulink] 无法写入 known_hosts（{error}），拒绝连接");
                return false;
            }
        };
        if file.write_all(entry.as_bytes()).is_err() {
            eprintln!("[rulink] known_hosts 写入失败，拒绝连接");
            return false;
        }
        eprintln!(
            "[rulink] 首次连接 {}，主机指纹已记入 {}",
            entry_host,
            path.display()
        );
        true
    }
}

impl client::Handler for SshTerminalHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match server_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => Ok(self.trust_key(key)),
            PublicKeyOrCertificate::Certificate(_) => {
                // 证书认证属于后续能力（见首发清单"安全与密钥"），首版一律拒绝。
                eprintln!("[rulink] 暂不支持 SSH 证书主机认证，拒绝连接");
                Ok(false)
            }
        }
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.feed
            .feed(format!("\r\n[rulink] {banner}\r\n").into_bytes());
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.side.contains(channel) {
            return Ok(());
        }
        self.feed.feed(data.to_vec());
        Ok(())
    }

    async fn extended_data(
        &mut self,
        channel: ChannelId,
        _ext: u32,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.side.contains(channel) {
            return Ok(());
        }
        // stderr 与 stdout 在远端 PTY 下本就是同一条流，统一灌入终端。
        self.feed.feed(data.to_vec());
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.side.contains(channel) {
            return Ok(());
        }
        self.feed.end_session();
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.side.contains(channel) {
            return Ok(());
        }
        self.feed.end_session();
        Ok(())
    }

    async fn exit_status(
        &mut self,
        channel: ChannelId,
        exit_status: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.side.contains(channel) {
            return Ok(());
        }
        log::info!("远端进程退出，状态码 {exit_status}");
        self.feed.end_session();
        Ok(())
    }
}
