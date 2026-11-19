//! SSH 引擎：russh 客户端事件处理与主机指纹校验（TOFU，正常记录存加密库
//! known_hosts 表；降级模式回退 `~/.ruterm/known_hosts` 文件）。
//!
//! 本模块只认识"字节"，不认识终端 UI：下行字节全部灌入
//! [`rgpui_term::FeedHandle`]，会话终止信号（EOF/关闭/退出码）折算成
//! `end_session`，由终端后端统一收尾。

use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rgpui_term::FeedHandle;
use russh::Channel;
use russh::ChannelId;
use russh::client::{self, ChannelOpenHandle, Msg, Session};
use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};
use tokio::sync::mpsc;

use crate::paths::StorageMode;
use crate::session::CwdHint;
use crate::vault::{Trust, Vault};

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

/// 一条进行中的 ZMODEM 上传的接管信息。
pub struct ZmodemTransfer {
    /// 上传任务的下行接管通道（`rz` 的 ZRINIT / ZDATA 应答都从这走）。
    pub downlink: mpsc::UnboundedSender<Vec<u8>>,
    /// 取消标记：UI 取消时置位，`send_file` 每轮循环检查后主动收场。
    pub cancel: Arc<AtomicBool>,
}

impl ZmodemTransfer {
    /// 请求取消：置位标记。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// ZMODEM（lrzsz）拖拽上传期间的下行接管点。
///
/// 安装时终端通道的输出不再喂给终端视图，而是转交上传任务（`rz` 的
/// ZRINIT/ZDATA 应答都在这条流里）；上传结束或被取消必须置回 None
/// 恢复终端。
pub type ZmodemSink = Arc<Mutex<Option<ZmodemTransfer>>>;

/// 新建一个未安装的接管点。
pub fn zmodem_sink() -> ZmodemSink {
    Arc::new(Mutex::new(None))
}

/// 终端通道登记。
///
/// `direct-tcpip` 旁路通道（端口转发 / 跳板）存在一个空窗：russh 处理完
/// 开通道确认包就唤醒调用方（仅调度、尚未运行），若对端「先说话」（telnet /
/// SMTP 等先发 banner 的服务，或跳板链下一跳的 sshd），第一个数据包可能已在
/// 同一轮被 `Handler::data` 收到——此时调用方还没来得及把通道登记进
/// `SideChannels`，字节会被灌进终端。登记终端通道 id 后，非终端通道的字节
/// 一概不进终端，空窗封死。
#[derive(Clone, Default)]
pub struct TerminalChannel(Arc<Mutex<TerminalChannelState>>);

/// 登记状态。
#[derive(Debug, Default)]
enum TerminalChannelState {
    /// 尚未登记：行为与旧版一致，所有非旁路字节都进终端（握手期的暂态）。
    #[default]
    Unregistered,
    /// 本连接没有终端（跳板机连接只跑 direct-tcpip 字节流）：
    /// 非旁路字节一律丢弃。
    Disabled,
    /// 已登记终端通道：只有该通道的字节进终端。
    Registered(ChannelId),
}

impl TerminalChannel {
    /// 登记终端通道 id（shell 通道打开后立即调用）。
    pub fn set(&self, channel: ChannelId) {
        *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            TerminalChannelState::Registered(channel);
    }

    /// 标记本连接没有终端通道（跳板机用）：所有非旁路字节都不进终端。
    pub fn disable(&self) {
        *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            TerminalChannelState::Disabled;
    }

    fn registered(&self) -> bool {
        !matches!(
            *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            TerminalChannelState::Unregistered
        )
    }

    fn is(&self, channel: ChannelId) -> bool {
        matches!(
            *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            TerminalChannelState::Registered(id) if id == channel
        )
    }
}

/// 降级模式（无加密库）使用的旧 known_hosts 文件路径。
pub fn legacy_known_hosts_path() -> PathBuf {
    StorageMode::detect().legacy_path(crate::paths::LEGACY_KNOWN_HOSTS_FILE)
}

/// OSC 7 状态机：在远端 shell 的下行字节流里识别 `ESC ] 7 ; file://host/path BEL`
/// （或 `ESC \` 终止），解码后作为目录线索回传 UI。
///
/// 跨 `data()` 调用保持状态，避免序列被分片时漏识别；超过 4KB 的 payload 直接丢弃，
/// 防止畸形序列把缓冲区撑爆。
#[derive(Default)]
struct Osc7Scan {
    state: Osc7State,
}

enum Osc7State {
    Idle { matched: u8 },
    Collecting { buf: Vec<u8>, esc_seen: bool },
}

impl Default for Osc7State {
    fn default() -> Self {
        Self::Idle { matched: 0 }
    }
}

const OSC7_PREFIX: &[u8] = b"\x1b]7;";

impl Osc7Scan {
    fn feed(&mut self, data: &[u8], cwd: &Option<mpsc::UnboundedSender<CwdHint>>) {
        for &b in data {
            match &mut self.state {
                Osc7State::Idle { matched } => {
                    let idx = *matched as usize;
                    if idx < OSC7_PREFIX.len() && b == OSC7_PREFIX[idx] {
                        *matched += 1;
                        if *matched as usize == OSC7_PREFIX.len() {
                            self.state = Osc7State::Collecting {
                                buf: Vec::new(),
                                esc_seen: false,
                            };
                        }
                    } else if b == OSC7_PREFIX[0] {
                        *matched = 1;
                    } else {
                        *matched = 0;
                    }
                }
                Osc7State::Collecting { buf, esc_seen } => {
                    if *esc_seen {
                        *esc_seen = false;
                        if b == b'\\' {
                            let payload = std::mem::take(buf);
                            self.state = Osc7State::Idle { matched: 0 };
                            Self::emit(&payload, cwd);
                        } else {
                            // ESC 不是 ST 终止的一部分，按字面量保留。
                            buf.push(0x1b);
                            buf.push(b);
                        }
                    } else if b == 0x07 {
                        let payload = std::mem::take(buf);
                        self.state = Osc7State::Idle { matched: 0 };
                        Self::emit(&payload, cwd);
                    } else if b == 0x1b {
                        *esc_seen = true;
                    } else if buf.len() < 4096 {
                        buf.push(b);
                    } else {
                        // 超长 payload：视为畸形，丢弃并复位。
                        self.state = Osc7State::Idle { matched: 0 };
                    }
                }
            }
        }
    }

    fn emit(payload: &[u8], cwd: &Option<mpsc::UnboundedSender<CwdHint>>) {
        let Some(tx) = cwd else { return };
        let Ok(s) = std::str::from_utf8(payload) else {
            return;
        };
        let Some(path) = crate::session::path_from_osc7(s) else {
            log::debug!("[Osc7Scan] 解析 OSC 7 失败: {:?}", s);
            return;
        };
        log::debug!("[Osc7Scan] 检测到 OSC 7: {}", path);
        let _ = tx.send(CwdHint::Report(path));
    }
}

/// 主机密钥的 SHA-256 指纹（hex 形式，避免依赖 base64 feature）。
fn fingerprint_hex(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256)
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 服务器为**远程转发**打开的通道：会话任务据此接上本机目标。
///
/// 通道由服务器发起，`connected_address/port` 是服务器上正在监听的地址与端口，
/// 用它匹配本地登记的转发规则；`originator_*` 是连到服务器的对端，仅用于日志。
pub struct ForwardedTcpip {
    pub channel: Channel<Msg>,
    pub connected_address: String,
    pub connected_port: u32,
    pub originator_address: String,
    pub originator_port: u32,
}

/// russh 会话的事件处理器：实现 `client::Handler`。
pub struct SshTerminalHandler {
    feed: FeedHandle,
    host: String,
    port: u16,
    /// 非终端通道（exec 探测等），其字节与控制消息都不进终端。
    side: SideChannels,
    /// 加密凭据库；None 表示降级模式，known_hosts 回退旧文件。
    vault: Option<Arc<Vault>>,
    /// 交互通道结束信号：让会话任务退出 `handle.await`，释放底层连接。
    ended: mpsc::UnboundedSender<()>,
    /// 远程转发通道交给会话任务统一接本地目标（Handler 里不放业务状态）。
    forwarded: mpsc::UnboundedSender<ForwardedTcpip>,
    /// 安装后终端通道的下行字节转交 lrzsz 上传任务（见 [`ZmodemSink`]）。
    zmodem: Option<ZmodemSink>,
    /// 目录线索回传：仅目标会话持有（跳板机为 None），下行字节经 OSC 7 扫描后投递。
    cwd: Option<mpsc::UnboundedSender<CwdHint>>,
    /// OSC 7 状态机：跨 `data()` 调用保持，处理分片到达的序列。
    osc7: Osc7Scan,
    /// 终端通道登记：非终端通道的字节不进终端（见 [`TerminalChannel`]）。
    terminal: TerminalChannel,
}

impl SshTerminalHandler {
    /// 以下行注入句柄、主机信息、旁路通道集合与可选凭据库构造处理器。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        feed: FeedHandle,
        host: String,
        port: u16,
        side: SideChannels,
        vault: Option<Arc<Vault>>,
        ended: mpsc::UnboundedSender<()>,
        forwarded: mpsc::UnboundedSender<ForwardedTcpip>,
        zmodem: Option<ZmodemSink>,
        cwd: Option<mpsc::UnboundedSender<CwdHint>>,
        terminal: TerminalChannel,
    ) -> Self {
        Self {
            feed,
            host,
            port,
            side,
            vault,
            ended,
            forwarded,
            zmodem,
            cwd,
            osc7: Osc7Scan::default(),
            terminal,
        }
    }

    /// 首次信任并持久化（Trust On First Use）：
    /// known_hosts 命中则校验，缺失则写入新记录并信任，不符则拒绝。
    fn trust_key(&mut self, key: &PublicKey) -> bool {
        let entry_host = format!("[{}]:{}", self.host, self.port);
        let fp = fingerprint_hex(key);
        let path = legacy_known_hosts_path();

        let existing = match crate::vault::read_legacy_known_hosts(&path) {
            Ok(text) => text.unwrap_or_default(),
            Err(error) => {
                log::warn!("{error}");
                return false;
            }
        };
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
            log::warn!(
                "{} 的主机密钥指纹与 known_hosts 不符，拒绝连接（可能是中间人攻击）",
                entry_host
            );
            return false;
        }

        // 首次接触该主机：写入记录并信任。
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::warn!("无法创建 {} 目录（{error}），拒绝连接", parent.display());
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
                log::warn!("无法写入 known_hosts（{error}），拒绝连接");
                return false;
            }
        };
        if file.write_all(entry.as_bytes()).is_err() {
            log::warn!("known_hosts 写入失败，拒绝连接");
            return false;
        }
        log::warn!("首次连接 {}，主机指纹已记入 {}", entry_host, path.display());
        true
    }
}

impl SshTerminalHandler {
    /// 终端通道的下行字节若正被 lrzsz 上传接管，就转交上传任务。
    ///
    /// 接收端已消失（上传任务结束/失败）时返回 false，让字节照常进终端，
    /// 避免把 shell 输出永久吞掉。
    fn divert_zmodem(&self, data: &[u8]) -> bool {
        let Some(sink) = self.zmodem.as_ref() else {
            return false;
        };
        let guard = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(transfer) = guard.as_ref() else {
            return false;
        };
        transfer.downlink.send(data.to_vec()).is_ok()
    }
}

impl client::Handler for SshTerminalHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match server_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                let Some(vault) = self.vault.clone() else {
                    // 降级模式（无加密库）：回退旧 known_hosts 文件逻辑。
                    return Ok(self.trust_key(key));
                };
                let entry_host = format!("[{}]:{}", self.host, self.port);
                let fp = fingerprint_hex(key);
                // fail-closed：库查询/写入任何失败都拒绝连接，绝不降级放行。
                match vault
                    .check_or_record_known_host(&entry_host, &fp, key.algorithm().as_ref())
                    .await
                {
                    Ok(Trust::Matched) => Ok(true),
                    Ok(Trust::New) => {
                        self.feed.feed(
                            format!("\r\n[ruterm] 首次连接 {entry_host}，主机指纹已记入加密库\r\n")
                                .into_bytes(),
                        );
                        Ok(true)
                    }
                    Ok(Trust::Mismatch) => {
                        self.feed.feed(
                            format!(
                                "\r\n[ruterm] 警告: {entry_host} 的主机密钥指纹与加密库记录不符，拒绝连接（可能是中间人攻击）\r\n"
                            )
                            .into_bytes(),
                        );
                        Ok(false)
                    }
                    Err(error) => {
                        self.feed.feed(
                            format!(
                                "\r\n[ruterm] 加密库 known_hosts 不可用（{error}），拒绝连接\r\n"
                            )
                            .into_bytes(),
                        );
                        Ok(false)
                    }
                }
            }
            PublicKeyOrCertificate::Certificate(_) => {
                // 证书认证属于后续能力（见首发清单"安全与密钥"），首版一律拒绝。
                log::warn!("暂不支持 SSH 证书主机认证，拒绝连接");
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
            .feed(format!("\r\n[ruterm] {banner}\r\n").into_bytes());
        Ok(())
    }

    /// 远程转发：服务器接受监听后，每来一个外部连接就会开一条 forwarded-tcpip 通道。
    ///
    /// 必须先登记旁路再交出去：通道里的字节由会话任务搬给本地目标，
    /// 同时不能让 `Handler::data` 把流量灌进终端。
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        self.side.add(channel.id());
        let _ = self.forwarded.send(ForwardedTcpip {
            channel,
            connected_address: connected_address.to_string(),
            connected_port,
            originator_address: originator_address.to_string(),
            originator_port,
        });
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
        // 终端通道登记后，只有它自己的字节进终端（见 [`TerminalChannel`]）。
        if self.terminal.registered() && !self.terminal.is(channel) {
            return Ok(());
        }
        // 先扫描 OSC 7（shell 工作目录上报），再决定是否转交 zmodem / 终端。
        self.osc7.feed(data, &self.cwd);
        if !self.divert_zmodem(data) {
            self.feed.feed(data.to_vec());
        }
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
        if self.terminal.registered() && !self.terminal.is(channel) {
            return Ok(());
        }
        // stderr 与 stdout 在远端 PTY 下本就是同一条流，统一灌入终端；
        // OSC 7 只走 stdout，但扫描一下也无害（非 file:// 前缀会被丢弃）。
        self.osc7.feed(data, &self.cwd);
        if !self.divert_zmodem(data) {
            self.feed.feed(data.to_vec());
        }
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
        let _ = self.ended.send(());
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
        let _ = self.ended.send(());
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
        let _ = self.ended.send(());
        Ok(())
    }
}
