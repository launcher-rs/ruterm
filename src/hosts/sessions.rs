//! 主机账号管理：本地持久化的会话列表（~/.ruterm/sessions.json）。
//!
//! 凭据策略：密码不入库，连接时临时输入；私钥只存路径。

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 认证方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMethod {
    /// SSH Agent（默认，免密免存）
    Agent,
    /// 私钥文件（存路径）
    IdentityFile,
    /// 密码（每次连接时弹窗输入，不落盘）
    Password,
}

/// 连接协议。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolKind {
    /// SSH（旧配置没有该字段时按它处理）。
    #[default]
    Ssh,
    /// Telnet：明文 TCP + 最小 IAC 协商，不做认证与加密。
    Telnet,
    /// 裸 TCP：把本机与 `host:port` 之间的字节双向搬运，上面不叠加任何协议
    /// （PuTTY 的 "Raw" 会话；连接串口服务器、redis-cli 式探测端口等）。
    Tcp,
    /// WebSocket：把终端字节跑在 ws://（或 wss://）帧里，对接 websockify /
    /// Web 串口网关这类"浏览器才能开终端"的服务（issue #3）。
    Ws,
    /// 串口：本地设备（COM3 / /dev/ttyUSB0），参数见 [`SerialParams`]。
    Serial,
    /// 远程桌面（RDP）：点开走桌面标签，不进终端。
    Rdp,
    /// 远程桌面（VNC）。
    Vnc,
}

impl ProtocolKind {
    pub fn label(self) -> &'static str {
        crate::protocols::protocol_spec(self).label
    }

    pub fn default_port(self) -> u16 {
        crate::protocols::protocol_spec(self).default_port
    }

    /// 桌面协议：点开主机行时走桌面标签，而不是终端会话。
    pub fn is_desktop(self) -> bool {
        crate::protocols::protocol_spec(self).desktop
    }
}

/// 一台可连接的主机账号。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostConfig {
    /// 连接协议。
    ///
    /// 加密库模式下不进 hosts 表（表结构固定），而是随 `settings` 表的
    /// `protocols` 键持久化，启动后回填到这里；降级模式直接随 sessions.json 落盘。
    #[serde(default)]
    pub protocol: ProtocolKind,
    /// 加密库中的主键（旧 JSON 数据迁移前为 0，入库时分配）
    #[serde(default)]
    pub id: i64,
    /// 展示名
    pub name: String,
    /// 主机地址
    pub host: String,
    /// 端口
    pub port: u16,
    /// 登录用户
    pub user: String,
    /// 认证方式
    pub auth: AuthMethod,
    /// 私钥路径（仅 IdentityFile 使用）
    #[serde(default)]
    pub key_path: String,
    /// 私钥是否内嵌在加密库（不落 JSON；读取时按库内是否存在私钥回填）。
    #[serde(skip)]
    pub key_inline: bool,
    /// 是否收藏（收藏主机在列表置顶，★ 标记）
    #[serde(default)]
    pub favorite: bool,
    /// 跳板机（指向另一台已保存主机的 id）；None 表示直连。
    ///
    /// 加密库模式下不存在 hosts 表里（避免 ALTER 迁移），而是随 `settings`
    /// 表的 `jumps` 键持久化，启动后回填到这里。
    #[serde(default)]
    pub proxy_jump: Option<i64>,
    /// 协议附加参数（串口波特率 / WebSocket 路径与 TLS）。
    ///
    /// 连接时直接读本字段：降级模式随 sessions.json 落盘（旧数据反序列化出
    /// 全默认值）；加密库模式 hosts 表没有该列，随 `settings` 表的
    /// `host_extras` 键持久化，启动后回填到这里（同 protocol / jump 的待遇）。
    #[serde(default)]
    pub extras: HostExtras,
}

/// 串口连接参数（MobaXterm 串口会话同款字段；随加密库 `host_extras` 键持久化）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialParams {
    /// 波特率（u16 装不下 230400 / 921600，所以独立用 u32 存）。
    pub baud: u32,
    /// 数据位（5–8）。
    pub data_bits: u8,
    /// 校验位。
    pub parity: SerialParity,
    /// 停止位（1 或 2）。
    pub stop_bits: u8,
    /// 流控。
    pub flow: SerialFlow,
}

impl Default for SerialParams {
    /// 现代开发板控制台的默认 115200 8-N-1，无流控。
    fn default() -> Self {
        Self {
            baud: 115_200,
            data_bits: 8,
            parity: SerialParity::None,
            stop_bits: 1,
            flow: SerialFlow::None,
        }
    }
}

/// 串口校验位。Mark / Space 仅 Windows 驱动支持，列进模型但不进表单选项。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SerialParity {
    #[default]
    None,
    Odd,
    Even,
    Mark,
    Space,
}

/// 串口流控：None、软件（XON/XOFF）、硬件（RTS/CTS）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SerialFlow {
    #[default]
    None,
    Software,
    Hardware,
}

/// WebSocket 会话参数（普通 TCP 字段表达不了路径与 TLS）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsParams {
    /// URL 路径，空串或不含前导 `/` 时按 `/` 处理。
    pub path: String,
    /// true 走 wss://（TLS）。
    pub tls: bool,
}

/// 每主机的协议附加参数。加密库 hosts 表结构固定，与 protocol / jump 一样
/// 不进列，随 `settings.host_extras` 键（JSON：主机 id → 参数）持久化，
/// 启动 / 开库后回填（`app::prefs::HostExtrasPrefs`）；降级模式存 settings.json。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostExtras {
    /// 串口参数（仅 Serial 协议有意义）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<SerialParams>,
    /// WebSocket 参数（仅 Ws 协议有意义）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws: Option<WsParams>,
}

impl HostExtras {
    /// 只保留对该协议有意义的参数：协议改掉后残留的另一类参数不算数，
    /// 迁移 / 合并时据此过滤，避免改协议后旧参数被当成有效值持久化。
    pub fn relevant_to(&self, protocol: ProtocolKind) -> Self {
        Self {
            serial: if protocol == ProtocolKind::Serial {
                self.serial
            } else {
                None
            },
            ws: if protocol == ProtocolKind::Ws {
                self.ws.clone()
            } else {
                None
            },
        }
    }

    /// 是否没有任何参数（没有就不写键，settings 里不留空记录）。
    pub fn is_empty(&self) -> bool {
        self.serial.is_none() && self.ws.is_none()
    }
}

impl HostConfig {
    /// 列表副标题（user@host:port；Telnet 没有登录用户时只写 host:port；
    /// 串口没有端口概念，只写设备路径）。
    pub fn addr(&self) -> String {
        if self.protocol == ProtocolKind::Serial {
            return self.host.clone();
        }
        if self.user.trim().is_empty() {
            format!("{}:{}", self.host, self.port)
        } else {
            format!("{}@{}:{}", self.user, self.host, self.port)
        }
    }

    /// 表单字段基本校验，返回首个错误描述。
    pub fn validate(&self) -> Option<String> {
        if self.host.trim().is_empty() {
            if self.protocol == ProtocolKind::Serial {
                return Some("设备路径不能为空".to_string());
            }
            return Some("主机地址不能为空".to_string());
        }
        // 串口没有端口概念（host 字段装的是设备路径，port 恒为 0）。
        if self.protocol != ProtocolKind::Serial && self.port == 0 {
            return Some(if self.protocol == ProtocolKind::Tcp {
                "裸 TCP 必须填写端口".to_string()
            } else {
                "端口无效".to_string()
            });
        }
        // Telnet 的登录名由服务端提示交互输入，表单里可以留空。
        if self.protocol == ProtocolKind::Ssh {
            if self.user.trim().is_empty() {
                return Some("用户名不能为空".to_string());
            }
            if self.auth == AuthMethod::IdentityFile
                && !self.key_inline
                && self.key_path.trim().is_empty()
            {
                return Some("请填写私钥路径，或改为内嵌私钥".to_string());
            }
        }
        // RDP 必须带账户名（IronRDP 不会在会话里追问）；VNC 多数只需密码。
        if self.protocol == ProtocolKind::Rdp && self.user.trim().is_empty() {
            return Some("用户名不能为空".to_string());
        }
        None
    }
}

/// 应用级设置（与主机列表分开存，避免动到 sessions.json 的数组格式）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// 主题模式："dark" / "light"；缺省按暗色。
    #[serde(default)]
    pub theme: String,
    /// 状态栏显示项偏好（序列化成 JSON 字符串；空串表示未设置）。
    #[serde(default)]
    pub status_bar: String,
    /// 终端外观偏好（字号 / 行高 / 配色方案，序列化成 JSON 字符串）。
    #[serde(default)]
    pub terminal: String,
    /// 会话行为偏好（自动重连 / 连接超时，序列化成 JSON 字符串）。
    #[serde(default)]
    pub session: String,
    /// 侧边栏分组（文件夹）偏好（序列化成 JSON 字符串）。
    #[serde(default)]
    pub sidebar: String,
    /// 本机 shell 自定义展示名（序列化成 JSON 字符串）。
    #[serde(default)]
    pub local_shells: String,
    /// 端口转发规则（按连接保存，序列化成 JSON 字符串）。
    #[serde(default)]
    pub forwards: String,
    /// 主机 → 跳板机映射（序列化成 JSON 字符串，键是主机 id）。
    #[serde(default)]
    pub jumps: String,
    /// 主机 → 协议映射（序列化成 JSON 字符串，键是主机 id；只记录非 SSH 的项）。
    #[serde(default)]
    pub protocols: String,
    /// 主机 → 协议附加参数（串口波特率等 / WebSocket 路径与 TLS；键是主机 id）。
    #[serde(default)]
    pub host_extras: String,
    /// 键宏（名称 / 触发键 / 步骤序列，序列化成 JSON 数组）。
    #[serde(default)]
    pub macros: String,
    /// 命令片段（名称 / 命令 / 备注，序列化成 JSON 数组）。
    #[serde(default)]
    pub snippets: String,
    /// 命令历史（命令行数组，新的在前）。
    #[serde(default)]
    pub history: String,
    /// 本机文件页偏好（隐藏文件开关 + 上次停留的目录，序列化成 JSON 字符串）。
    #[serde(default)]
    pub local_files: String,
    /// 检查更新偏好（镜像开关 + 自定义镜像前缀，序列化成 JSON 字符串）。
    #[serde(default)]
    pub update: String,
    /// 是否启用系统托盘；缺省关闭（关窗即退出）。
    #[serde(default)]
    pub tray: bool,
}

impl Settings {
    /// 是否暗色（非 "light" 一律视为暗色）。
    pub fn dark(&self) -> bool {
        self.theme != "light"
    }
}

/// 会话仓库：整表读写 JSON 文件。
pub struct SessionStore {
    path: PathBuf,
    /// 应用级设置文件路径（settings.json）。
    settings_path: PathBuf,
    /// 界面语言启动镜像（locale.txt）。刻意不放进 settings.json：
    /// 开库迁移会把 settings.json 当遗留文件归档并回灌各键，
    /// 届时镜像里未设置的 `theme` 会抹掉库内主题。
    locale_path: PathBuf,
    /// 明暗主题启动镜像；主题不是秘密，锁库时也可用于首帧渲染。
    theme_path: PathBuf,
}

impl SessionStore {
    /// 按存储模式构造旧 JSON 仓库（便携=data/，全局=~/.ruterm；降级路径保留）。
    pub fn new(mode: &crate::paths::StorageMode) -> Self {
        let dir = mode.dir();
        Self {
            path: dir.join(crate::paths::LEGACY_SESSIONS_FILE),
            settings_path: dir.join(crate::paths::LEGACY_SETTINGS_FILE),
            locale_path: dir.join(crate::paths::LOCALE_FILE),
            theme_path: dir.join(crate::paths::THEME_FILE),
        }
    }

    /// 读取应用级设置；文件缺失或损坏时返回默认值（暗色）。
    /// 损坏只影响本次读取，回写时 [`Self::update_settings`] 会拒绝覆盖原文件。
    pub fn load_settings(&self) -> Settings {
        match self.read_settings_file() {
            Ok(Some(settings)) => settings,
            Ok(None) => Settings::default(),
            Err(error) => {
                log::warn!("{error}；本次按默认设置启动");
                Settings::default()
            }
        }
    }

    /// 严格读取：缺失返回 None，损坏或不可读返回错误（避免把坏文件当空设置覆盖）。
    fn read_settings_file(&self) -> Result<Option<Settings>, String> {
        match std::fs::read_to_string(&self.settings_path) {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|error| {
                format!(
                    "settings.json 解析失败（{}）：{error}",
                    self.settings_path.display()
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!(
                "读取 {} 失败：{error}",
                self.settings_path.display()
            )),
        }
    }

    /// 先读启动镜像；旧版本没有镜像时兼容 settings.json，默认暗色。
    pub fn load_theme(&self) -> bool {
        match std::fs::read_to_string(&self.theme_path) {
            Ok(theme) if theme.trim() == "light" => false,
            Ok(theme) if theme.trim() == "dark" => true,
            _ => {
                let legacy = std::fs::read_to_string(&self.settings_path)
                    .ok()
                    .and_then(|text| serde_json::from_str::<Settings>(&text).ok());
                let archived = self.settings_path.with_file_name("settings.json.migrated");
                let settings = legacy.or_else(|| {
                    std::fs::read_to_string(archived)
                        .ok()
                        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
                });
                let dark = settings.is_none_or(|settings| settings.dark());
                self.save_theme_mirror(dark);
                dark
            }
        }
    }

    /// 就地改一项设置后整体回写（读-改-写，避免各设置项互相覆盖）。
    ///
    /// settings.json 已损坏或不可读时拒绝写入：宁可这次设置不生效，
    /// 也不把用户其余设置连同坏文件一起覆盖掉。
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut settings = match self.read_settings_file() {
            Ok(Some(settings)) => settings,
            Ok(None) => Settings::default(),
            Err(error) => {
                log::error!("{error}；拒绝覆盖 settings.json");
                return;
            }
        };
        f(&mut settings);
        match serde_json::to_string_pretty(&settings) {
            Ok(text) => {
                if let Err(error) = write_atomic(&self.settings_path, text.as_bytes()) {
                    log::warn!("写入 settings.json 失败: {error}");
                }
            }
            Err(error) => log::warn!("序列化设置失败: {error}"),
        }
    }

    /// 保存主题选择（"dark" / "light"）；失败仅记日志。
    pub fn save_theme(&self, dark: bool) {
        self.update_settings(|settings| {
            settings.theme = if dark { "dark" } else { "light" }.to_string();
        });
        self.save_theme_mirror(dark);
    }

    /// 仅更新加密库锁定期间可读取的非敏感主题启动镜像。
    pub fn save_theme_mirror(&self, dark: bool) {
        if let Err(error) = write_atomic(
            &self.theme_path,
            if dark { "dark" } else { "light" }.as_bytes(),
        ) {
            log::warn!("写入 theme.txt 失败: {error}");
        }
    }

    /// 保存托盘开关（启动时要读，统一放 settings.json）。
    pub fn save_tray(&self, enabled: bool) {
        self.update_settings(|settings| settings.tray = enabled);
    }

    /// 降级模式（无加密库）下保存状态栏偏好。
    pub fn save_status_bar(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.status_bar = json);
    }

    /// 降级模式（无加密库）下保存终端外观偏好。
    pub fn save_terminal(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.terminal = json);
    }

    /// 降级模式（无加密库）下保存会话行为偏好。
    pub fn save_session(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.session = json);
    }

    /// 降级模式（无加密库）下保存侧边栏分组偏好。
    pub fn save_sidebar(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.sidebar = json);
    }

    /// 降级模式（无加密库）下保存本机 shell 自定义名。
    pub fn save_local_shells(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.local_shells = json);
    }

    /// 降级模式（无加密库）下保存检查更新偏好。
    pub fn save_update(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.update = json);
    }

    /// 降级模式（无加密库）下保存本机文件页偏好。
    pub fn save_local_files(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.local_files = json);
    }

    /// 降级模式（无加密库）下保存端口转发规则。
    pub fn save_forwards(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.forwards = json);
    }

    /// 降级模式（无加密库）下保存跳板机映射。
    pub fn save_jumps(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.jumps = json);
    }

    /// 降级模式（无加密库）下保存主机协议映射。
    pub fn save_protocols(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.protocols = json);
    }

    /// 降级模式（无加密库）下保存主机协议附加参数。
    pub fn save_host_extras(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.host_extras = json);
    }

    /// 降级模式（无加密库）下保存键宏列表。
    pub fn save_macros(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.macros = json);
    }

    /// 降级模式（无加密库）下保存命令片段。
    pub fn save_snippets(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.snippets = json);
    }

    /// 降级模式（无加密库）下保存命令历史。
    pub fn save_history(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.history = json);
    }

    /// 读取界面语言启动镜像；缺失或值不认识时按中文（界面原文语言）。
    pub fn load_language(&self) -> String {
        let text = std::fs::read_to_string(&self.locale_path).unwrap_or_default();
        crate::i18n::normalize(text.trim()).to_string()
    }

    /// 保存界面语言偏好到启动镜像；失败仅记日志（库内那份才是权威值）。
    pub fn save_language(&self, locale: &str) {
        if let Err(error) = write_atomic(&self.locale_path, locale.as_bytes()) {
            log::warn!("写入 {} 失败: {error}", self.locale_path.display());
        }
    }

    /// 迁移完成后把 sessions.json 改名为 sessions.json.migrated（保留现场，不删除）。
    pub fn mark_migrated(&self) {
        if !self.path.exists() {
            return;
        }
        let target = self.path.with_extension("json.migrated");
        if target.exists() {
            log::warn!(
                "{} 已存在，保留 {} 不重复改名",
                target.display(),
                self.path.display()
            );
            return;
        }
        if let Err(error) = std::fs::rename(&self.path, &target) {
            log::warn!("迁移 sessions.json 到加密库后改名失败: {error}");
        }
    }

    /// 读取主机列表；文件缺失或损坏时返回空列表（损坏文件记日志保留现场）。
    /// 损坏时 [`Self::save`] 会拒绝覆盖，避免一次空列表把现场抹掉。
    pub fn load(&self) -> Vec<HostConfig> {
        match self.read_hosts_file() {
            Ok(Some(hosts)) => hosts,
            Ok(None) => Vec::new(),
            Err(error) => {
                log::warn!("{error}；本次以空列表启动");
                Vec::new()
            }
        }
    }

    /// 严格读取：缺失返回 None，损坏或不可读返回错误。
    fn read_hosts_file(&self) -> Result<Option<Vec<HostConfig>>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|error| {
                format!("sessions.json 解析失败（{}）：{error}", self.path.display())
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("读取 {} 失败：{error}", self.path.display())),
        }
    }

    /// 整表写回；失败仅记日志（不阻断 UI 操作）。
    ///
    /// 写前再确认原文件可解析：用户手改坏的文件绝不被内存里的列表覆盖。
    pub fn save(&self, hosts: &[HostConfig]) {
        if let Err(error) = self.read_hosts_file() {
            log::error!("{error}；拒绝覆盖 sessions.json");
            return;
        }
        match serde_json::to_string_pretty(hosts) {
            Ok(text) => {
                if let Err(error) = write_atomic(&self.path, text.as_bytes()) {
                    log::warn!("写入 sessions.json 失败: {error}");
                }
            }
            Err(error) => log::warn!("序列化会话列表失败: {error}"),
        }
    }
}

/// 同目录临时文件 + fsync + 原子替换：半截写入不会留下损坏的 JSON。
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings".into());
    let temp = parent.join(format!(".{name}.{}.{stamp}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn store(tag: &str) -> SessionStore {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ruterm-sessions-{tag}-{}-{stamp}",
            std::process::id()
        ));
        SessionStore {
            path: dir.join("sessions.json"),
            settings_path: dir.join("settings.json"),
            locale_path: dir.join("locale.txt"),
            theme_path: dir.join("theme.txt"),
        }
    }

    fn host(name: &str) -> HostConfig {
        HostConfig {
            protocol: ProtocolKind::Ssh,
            id: 1,
            name: name.to_string(),
            host: "example.com".to_string(),
            port: 22,
            user: "root".to_string(),
            auth: AuthMethod::Agent,
            key_path: String::new(),
            key_inline: false,
            favorite: false,
            proxy_jump: None,
            extras: Default::default(),
        }
    }

    #[test]
    fn missing_files_are_treated_as_empty() {
        let store = store("missing");
        assert!(store.load().is_empty());
        assert_eq!(store.load_settings().theme, "");
    }

    #[test]
    fn hosts_roundtrip_through_atomic_write() {
        let store = store("hosts-roundtrip");
        store.save(&[host("one")]);
        let loaded = store.load();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "one");
        for entry in std::fs::read_dir(store.path.parent().unwrap()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".tmp"), "临时文件残留：{name}");
        }
    }

    #[test]
    fn corrupt_hosts_file_is_not_overwritten() {
        let store = store("hosts-corrupt");
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(&store.path, b"[{\"name\":").unwrap();
        assert!(store.load().is_empty());
        store.save(&[host("replacement")]);
        assert_eq!(std::fs::read(&store.path).unwrap(), b"[{\"name\":");
    }

    #[test]
    fn corrupt_settings_file_is_not_overwritten() {
        let store = store("settings-corrupt");
        std::fs::create_dir_all(store.settings_path.parent().unwrap()).unwrap();
        let original = b"{\"theme\": ";
        std::fs::write(&store.settings_path, original).unwrap();
        assert_eq!(store.load_settings().theme, "");
        store.update_settings(|settings| settings.theme = "light".to_string());
        assert_eq!(std::fs::read(&store.settings_path).unwrap(), original);
    }

    #[test]
    fn settings_roundtrip_and_theme_mirror() {
        let store = store("settings-roundtrip");
        store.update_settings(|settings| settings.theme = "light".to_string());
        assert_eq!(store.load_settings().theme, "light");
        store.save_theme_mirror(true);
        assert!(store.load_theme());
        store.save_language("en");
        assert_eq!(store.load_language(), "en");
    }

    #[test]
    fn serial_addr_shows_device_only() {
        let mut device = host("控制台");
        device.protocol = ProtocolKind::Serial;
        device.host = "COM3".to_string();
        device.port = 0;
        assert_eq!(device.addr(), "COM3");
        assert_eq!(
            crate::protocols::protocol_spec(ProtocolKind::Serial).default_port,
            0
        );
    }

    #[test]
    fn serial_validate_needs_device_but_not_port() {
        let mut device = host("控制台");
        device.protocol = ProtocolKind::Serial;
        device.host = "/dev/ttyUSB0".to_string();
        device.port = 0;
        assert_eq!(device.validate(), None);
        // 设备路径缺失要报错，且文案与主机地址的措辞区分开。
        device.host = " ".to_string();
        assert_eq!(device.validate().as_deref(), Some("设备路径不能为空"));
    }

    #[test]
    fn tcp_validate_requires_a_port() {
        let mut target = host("网关");
        target.protocol = ProtocolKind::Tcp;
        target.port = 0;
        assert_eq!(target.validate().as_deref(), Some("裸 TCP 必须填写端口"));
        target.port = 7681;
        assert_eq!(target.validate(), None);
    }

    #[test]
    fn extras_roundtrip_and_relevance_filter() {
        let mut device = host("控制台");
        device.protocol = ProtocolKind::Serial;
        device.extras = HostExtras {
            serial: Some(SerialParams::default()),
            ws: Some(WsParams {
                path: "/ws".into(),
                tls: true,
            }),
        };
        // 改成 SSH 后串口参数不再有意义：relevant_to 只保留对应协议那份。
        assert_eq!(
            device.extras.relevant_to(ProtocolKind::Ssh),
            HostExtras::default()
        );
        assert_eq!(
            device
                .extras
                .relevant_to(ProtocolKind::Serial)
                .serial,
            Some(SerialParams::default())
        );
        // 落盘再读回：旧版本 JSON 没有 extras 字段时按全默认处理。
        let store = store("extras-roundtrip");
        store.save(&[device]);
        let loaded = store.load();
        assert_eq!(loaded[0].extras.serial, Some(SerialParams::default()));
        let legacy: Vec<HostConfig> = serde_json::from_str(
            r#"[{"name":"旧数据","host":"h","port":22,"user":"u","auth":"Agent"}]"#,
        )
        .unwrap();
        assert_eq!(legacy[0].extras, HostExtras::default());
    }
}
