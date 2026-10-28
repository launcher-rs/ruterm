//! RuLink：基于 rgpui + rgpui-term 外接 IO 后端的 SSH 客户端。
//!
//! 形态为 GUI 会话管理器：无参数启动即进入主界面，左侧是持久化的主机账号列表，
//! 点击账号即连接；密码认证的账号在连接时临时输入密码（绝不落盘），私钥/Agent
//! 认证直接连接。仍兼容一次性命令行快速连接（带目标参数时首启动即连上该主机）。

mod config;
mod local_shell;
mod paths;
mod remote;
mod session;
mod sessions;
mod ssh;
mod ssh_config;
mod transfer;
mod vault;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rgpui::components::Select;
use rgpui::components::split_pane::{CollapsiblePane, SplitDirection, SplitPane, SplitPaneState};
use rgpui::components::status_bar::{StatusBar, StatusBarItem};
use rgpui::input_ui::{Input, InputContentType, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::title_bar::TITLE_BAR_HEIGHT;
use rgpui::{
    AbsoluteLength, AnyElement, App, Bounds, Button, ButtonVariants, Checkbox, ClickEvent, Context,
    ContextMenuExt, Div, DropdownMenu, Entity, FocusHandle, Focusable, FontWeight, Hsla, Icon,
    IconName, Image, ImageFormat, InteractiveElement, IntoElement, KeyBinding, KeyDownEvent,
    MouseButton, NoAction, ParentElement, Pixels, PopupMenu, PopupMenuItem, Render, Rgba,
    ScrollHandle, ScrollableElement, SharedString, Stateful, StatefulInteractiveElement, Styled,
    TrayMenuItem, Window, WindowBounds, WindowControlArea, WindowOptions, actions, div, h_flex,
    img, px, size, v_flex,
};
use rgpui_term::{
    Event as TerminalEvent, TerminalBuilder, TerminalThemeConfig, TerminalView, TextStyle,
    ThemeDefinition, ThemeManager,
};
use serde::{Deserialize, Serialize};

use crate::config::SshTarget;
use crate::local_shell::ShellProfile;
use crate::paths::StorageMode;
use crate::remote::{Metric, RemoteInfo};
use crate::sessions::{AuthMethod, HostConfig, SessionStore};
use crate::vault::Vault;

// 应用级动作：既绑到全局快捷键，也挂在标题栏菜单项上。rgpui 会按键绑定
// 在菜单项右侧自动渲染按键（`PopupMenu::render_key_binding`），因此不再需要在
// 「帮助」菜单里用纯文字罗列快捷键。
actions!(
    rulink,
    [
        ToggleSidebar,
        ToggleTheme,
        NewHost,
        NewLocalTerminal,
        NextTab,
        PrevTab,
        OpenSettings,
        OpenTransfer,
        OpenForward,
        SplitRight,
        SplitDown,
        CloseSplit
    ]
);

/// 认证方式的展示名。
fn auth_label(method: AuthMethod) -> &'static str {
    match method {
        AuthMethod::Agent => "SSH Agent",
        AuthMethod::IdentityFile => "私钥",
        AuthMethod::Password => "密码",
    }
}

/// 合并两条状态附注（保存成功时的括注）。
fn join_notes(first: Option<String>, second: Option<String>) -> String {
    let mut joined = String::new();
    if let Some(first) = first {
        joined.push_str(&first);
    }
    if let Some(second) = second {
        joined.push_str(&second);
    }
    joined
}

/// 一个已连接的会话标签（持有终端视图）。
struct SessionTab {
    /// 稳定标签 id（会话结束或用户关闭时据此定位）。
    id: usize,
    /// 标签标题（主机名）。
    title: SharedString,
    /// 连接位置（user@host:port 或本地 shell 的路径），状态栏展示。
    addr: SharedString,
    /// 认证方式，状态栏展示；本地终端没有认证概念，为 None。
    auth: Option<AuthMethod>,
    /// 远程服务器基本信息（连接后经 exec 探测异步填充，探测失败保持 None）。
    remote_info: Option<RemoteInfo>,
    /// 渲染用终端视图。
    terminal_view: Entity<TerminalView>,
    /// 会话已结束（远端 exit / 断线 / 连接失败）。标签保留终端内容并显示重连条。
    ended: bool,
    /// 结束原因是失败（连接失败 / 异常中断）：状态栏文案由失败路径负责，不被"已断开"覆盖。
    failed: bool,
    /// 重连所需参数；本地终端没有会话概念，为 None。
    resume: Option<Resume>,
    /// 主动断开当前 SSH 会话（关闭标签时发送；本地终端为 None）。
    disconnect: Option<tokio::sync::oneshot::Sender<()>>,
    /// 会话指令通道（SFTP 传输 / 端口转发；本地终端为 None）。
    commands: Option<tokio::sync::mpsc::UnboundedSender<session::SessionCommand>>,
    /// 本标签当前生效的端口转发规则（随会话结束清空，不持久化）。
    forwards: Vec<session::ForwardRule>,
    /// 本轮连续自动重连已尝试次数（0＝不在自动重连中，手动重连会清零）。
    auto_retry_attempt: u32,
    /// 已排定一次自动重连（横幅展示等待与取消入口）。
    auto_retry_pending: bool,
    /// 本次会话 shell 成功开启的时刻：稳定运行过一阵后再断线，自动重连次数重新计。
    connected_at: Option<Instant>,
}

/// 分屏视图：主区同时显示两个会话标签（左/上＝first，右/下＝second）。
///
/// 只把"哪两个标签并排"记成一个固定有序对：点哪一侧就切到哪个标签，
/// 但两屏位置不互换；从标签栏切到第三个会话时自动退出分屏。
struct SplitView {
    first: usize,
    second: usize,
    direction: SplitDirection,
    /// 可拖拽的分隔条状态（比例只存活在内存，每次分屏从 50% 开始）。
    pane: Entity<SplitPaneState>,
}

/// 文件传输弹窗：本地路径 + 远端路径，上传与下载共用。
struct TransferDialog {
    /// 目标会话标签 id（传输指令发往该标签的 SSH 连接）。
    tab_id: usize,
    /// 弹窗标题里的会话名。
    title: SharedString,
    local: Entity<InputState>,
    remote: Entity<InputState>,
    /// SFTP 查询到的远端主目录；用于预填上传落点。
    home: Option<String>,
    /// 传输进行中：按钮禁用、进度可见。
    busy: bool,
    direction: Option<transfer::Direction>,
    /// (已传输, 总量)
    progress: Option<(u64, u64)>,
    error: Option<SharedString>,
    info: Option<SharedString>,
}

/// 端口转发弹窗：默认是规则列表，点「设置…」才展开新增 / 编辑表单。
struct ForwardDialog {
    /// 目标会话标签 id（转发指令发往该标签的 SSH 连接）。
    tab_id: usize,
    /// 弹窗标题里的会话名。
    title: SharedString,
    /// 该会话的连接键（user@host:port），保存规则时按它归档。
    addr: SharedString,
    /// 表单是否展开（列表与新增分开，避免列表被表单挤占）。
    editing: bool,
    /// 当前方向（-L / -R / -D），切换只换标签语义，不改输入内容。
    kind: session::ForwardKind,
    /// 新规则的备注名（可空）。
    name: Entity<InputState>,
    listen_port: Entity<InputState>,
    target_host: Entity<InputState>,
    target_port: Entity<InputState>,
    /// 新规则保存时是否带「连接时自动开启」。
    auto_start: bool,
    error: Option<SharedString>,
    info: Option<SharedString>,
}

/// 断线标签的重连参数快照。
#[derive(Clone)]
struct Resume {
    /// 原连接目标（含仅内存持有的密码/口令，绝不落盘）。
    target: SshTarget,
    /// 来源主机（None＝命令行快速连接）；重连时优先用列表里的最新配置。
    host: Option<HostConfig>,
    /// 秘密是否取自加密库：重连仍失败时据此清掉库里的坏凭据。
    secret_from_vault: bool,
}

/// 新建/编辑主机表单（各字段为受控输入状态）。
struct NewHostForm {
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    key_path: Entity<InputState>,
    /// 私钥来源：true＝内嵌到加密库（PEM 存库），false＝本地文件路径。
    key_inline: bool,
    /// 本次表单里新导入的私钥 PEM（仅内存；保存时才写入加密库）。
    key_data: Option<String>,
    /// 导入的私钥文件名，仅用于界面提示（绝不展示内容）。
    key_label: Option<String>,
    /// 加密私钥口令（可选）。保存时勾选入库则写加密库，否则仅本次内存缓存；绝不落明文。
    passphrase: Entity<InputState>,
    /// 表单层"保存口令到加密库"开关（默认否＝仅本次内存持有）。
    save_secret: bool,
    auth: AuthMethod,
    /// 保存到的分组；None＝未分组。从分组右键进入时预填该组，编辑时预填原归属。
    group: Option<SharedString>,
    /// 跳板机（另一台已保存主机的 id）；None＝直连。
    jump: Option<i64>,
    /// Some(index)＝编辑既有主机（保存时按该下标原位替换）。
    editing: Option<usize>,
}

/// 分组新建 / 重命名弹窗状态。
#[derive(Clone)]
struct GroupPrompt {
    /// Some(原名)＝重命名既有分组；None＝新建。
    renaming: Option<SharedString>,
    name: Entity<InputState>,
}

/// 本地 shell 弹窗（重命名 / 设置起始目录）状态。
struct ShellPrompt {
    /// profile id（路径 + 参数）；扫描重建列表后仍能定位。
    id: String,
    kind: ShellPromptKind,
    /// 系统默认名或默认目录文本，用于「留空恢复默认」提示。
    default_text: String,
    input: Entity<InputState>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShellPromptKind {
    Rename,
    WorkingDirectory,
    Arguments,
    Environment,
}

impl ShellPromptKind {
    /// 环境变量是多行文本（每行一条），其余为单行。
    fn multi_line(self) -> bool {
        matches!(self, Self::Environment)
    }
}

/// 弹窗索取的秘密类型：登录密码或加密私钥的口令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptKind {
    Password,
    Passphrase,
}

/// 待输入密码/口令后连接的会话（点击需要秘密的主机时进入）。
struct PendingConnect {
    host: HostConfig,
    password: Entity<InputState>,
    /// 是否勾选"保存"（仅加密库可用时生效，默认否＝本次内存持有）。
    save_password: bool,
    /// 弹窗类型（决定文案与连接参数落点）。
    kind: PromptKind,
}

/// 便携/主密码库的解锁弹窗状态（主密码派生密钥，库文件可带往他机）。
struct UnlockState {
    passphrase: Entity<InputState>,
    /// 首次建库：需要二次输入确认主密码（口令错误将无法重建库）。
    confirm: Entity<InputState>,
    is_new: bool,
    error: Option<SharedString>,
}

/// 设置弹窗状态：主密码表单（含当前口令校验）与应用偏好。
struct SettingsState {
    /// 当前分页（见 `SETTINGS_TAB_*`）。
    tab: usize,
    /// 已是口令档时要求的"当前主密码"——应用可能在托盘里挂很久，
    /// 不验证就能换密钥等于没有锁。库未解锁时不显示。
    current: Option<Entity<InputState>>,
    /// 新口令两栏：提交成功后连同输入一并丢弃，明文不留存界面里；
    /// 需要再改一次时点"更改主密码"重建输入框。
    password: Option<Entity<InputState>>,
    confirm: Option<Entity<InputState>>,
    error: Option<SharedString>,
    info: Option<SharedString>,
    /// 搬迁进行中：输入框与保存按钮禁用，避免重复触发。
    busy: bool,
}

/// 主密码操作：设置 / 更换为口令派生密钥，或取消并换回系统凭据管理器密钥。
#[derive(Clone, Copy, PartialEq, Eq)]
enum MasterPasswordAction {
    Set,
    Cancel,
}

/// 新口令输入框的提示语：口令档下这次操作是"更换"。
fn password_placeholder(passphrase_mode: bool) -> &'static str {
    if passphrase_mode {
        "新主密码"
    } else {
        "主密码（可不设置）"
    }
}

/// 设置弹窗分页：凭据库与主密码。
const SETTINGS_TAB_VAULT: usize = 0;
/// 设置弹窗分页：外观。
const SETTINGS_TAB_APPEARANCE: usize = 1;
/// 设置弹窗分页：状态栏显示项。
const SETTINGS_TAB_STATUS: usize = 2;
/// 设置弹窗分页：终端外观（字号 / 行高 / 配色方案）。
const SETTINGS_TAB_TERMINAL: usize = 3;
/// 设置弹窗分页：会话行为（自动重连 / 连接超时）。
const SETTINGS_TAB_SESSION: usize = 4;

/// 「会话」菜单里最近连接最多保留几条；再多菜单就太长了。
const RECENT_HOSTS_MAX: usize = 8;

/// 设置弹窗固定宽高：切分页不重排，弹窗不随内容忽大忽小。
const SETTINGS_DIALOG_W: f32 = 640.0;
const SETTINGS_DIALOG_H: f32 = 470.0;
/// 内容区高度：弹窗高 − 上下内边距（20×2）− 标题行 − 两条间距 − 底部按钮行。
/// 超出部分由内容区自身滚动，不再把弹窗撑高。
const SETTINGS_CONTENT_H: f32 = 352.0;
/// 左侧分页导航宽度。
const SETTINGS_NAV_W: f32 = 148.0;

/// 状态栏显示项偏好：条目多寡因人的使用习惯差别很大，交给用户勾选。
///
/// 远程指标按 `Metric::id()` 记账，缺项一律视为显示（新增指标默认出现，
/// 老配置不会把它悄悄关掉）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusBarPrefs {
    /// 活动会话地址 `user@host:port`。
    #[serde(default = "shown")]
    pub addr: bool,
    /// 认证方式徽标。
    #[serde(default = "shown")]
    pub auth: bool,
    /// 远程指标开关（键为 `Metric::id()`）。
    #[serde(default)]
    pub metrics: HashMap<String, bool>,
}

/// 状态栏里可勾选的一个显示项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusPref {
    Addr,
    Auth,
    Metric(Metric),
}

/// 反序列化缺字段时的默认：显示。
fn shown() -> bool {
    true
}

impl Default for StatusBarPrefs {
    fn default() -> Self {
        Self {
            addr: true,
            auth: true,
            metrics: HashMap::new(),
        }
    }
}

impl StatusBarPrefs {
    /// 该远程指标是否显示（未记录即显示）。
    fn metric_on(&self, metric: Metric) -> bool {
        *self.metrics.get(metric.id()).unwrap_or(&true)
    }

    fn set_metric(&mut self, metric: Metric, on: bool) {
        self.metrics.insert(metric.id().to_string(), on);
    }

    /// 序列化为入库/入文件的字符串。
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 解析库内 / settings.json 里的偏好；损坏或为空时回默认（全显示）。
    fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 终端字号范围与步进：下限保证可读，上限避免把窗口撑爆。
const FONT_SIZE_MIN: f32 = 10.0;
const FONT_SIZE_MAX: f32 = 32.0;
const FONT_SIZE_STEP: f32 = 1.0;
const FONT_SIZE_DEFAULT: f32 = 14.0;
/// 行高倍数范围与步进（过密会糊字，过疏浪费竖向空间）。
const LINE_HEIGHT_MIN: f32 = 1.0;
const LINE_HEIGHT_MAX: f32 = 2.0;
const LINE_HEIGHT_STEP: f32 = 0.1;
const LINE_HEIGHT_DEFAULT: f32 = 1.2;
/// 配色方案默认值，与 rgpui-term `ThemeManager` 的初始主题一致。
const TERMINAL_THEME_DEFAULT: &str = "One Dark";
/// 自定义配色在 `ThemeManager` 里注册的名字，也是 `TerminalPrefs::theme` 的一个取值。
const TERMINAL_THEME_CUSTOM: &str = "自定义";
/// 自定义配色可编辑的字段数：前景、背景、8 个普通色、8 个明亮色。
const CUSTOM_COLOR_FIELDS: usize = 18;
/// 普通色在 `TerminalColors` 字段序列里的起点（前两个是前景 / 背景）。
const CUSTOM_ANSI_BASE: usize = 2;
/// 明亮色起点（紧跟 8 个普通色）。
const CUSTOM_BRIGHT_BASE: usize = CUSTOM_ANSI_BASE + 8;
/// ANSI 前 8 色的中文名，按「黑 红 绿 黄 蓝 洋红 青 白」排列。
const ANSI_COLOR_NAMES: [&str; 8] = ["黑", "红", "绿", "黄", "蓝", "洋红", "青", "白"];
/// 自定义配色的初始值：与默认方案 One Dark 一致，用户切到「自定义」时不会突然换色。
const CUSTOM_DEFAULT_FOREGROUND: &str = "#ABB2BF";
const CUSTOM_DEFAULT_BACKGROUND: &str = "#282C34";
const CUSTOM_DEFAULT_ANSI: [&str; 8] = [
    "#3F4451", "#E06C75", "#98C379", "#E5C07B", "#61AFEF", "#C678DD", "#56B6C2", "#ABB2BF",
];
const CUSTOM_DEFAULT_BRIGHT: [&str; 8] = [
    "#5C6370", "#E06C75", "#98C379", "#E5C07B", "#61AFEF", "#C678DD", "#56B6C2", "#FFFFFF",
];

/// 自定义终端配色：前景 / 背景 + 16 色（普通 8 + 明亮 8），一律存 `#RRGGBB`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalColors {
    pub foreground: String,
    pub background: String,
    pub ansi: [String; 8],
    pub bright: [String; 8],
}

impl Default for TerminalColors {
    fn default() -> Self {
        Self {
            foreground: CUSTOM_DEFAULT_FOREGROUND.to_string(),
            background: CUSTOM_DEFAULT_BACKGROUND.to_string(),
            ansi: CUSTOM_DEFAULT_ANSI.map(str::to_string),
            bright: CUSTOM_DEFAULT_BRIGHT.map(str::to_string),
        }
    }
}

impl TerminalColors {
    /// 读取第 `index` 个可编辑字段（顺序见 `CUSTOM_*_BASE` 常量）。
    fn hex_field(&self, index: usize) -> &str {
        match index {
            0 => &self.foreground,
            1 => &self.background,
            i if (CUSTOM_ANSI_BASE..CUSTOM_BRIGHT_BASE).contains(&i) => {
                &self.ansi[i - CUSTOM_ANSI_BASE]
            }
            i if (CUSTOM_BRIGHT_BASE..CUSTOM_COLOR_FIELDS).contains(&i) => {
                &self.bright[i - CUSTOM_BRIGHT_BASE]
            }
            _ => CUSTOM_DEFAULT_FOREGROUND,
        }
    }

    /// 写入第 `index` 个字段；值须已通过 `normalize_hex`。
    fn set_hex_field(&mut self, index: usize, value: String) {
        match index {
            0 => self.foreground = value,
            1 => self.background = value,
            i if (CUSTOM_ANSI_BASE..CUSTOM_BRIGHT_BASE).contains(&i) => {
                self.ansi[i - CUSTOM_ANSI_BASE] = value;
            }
            i if (CUSTOM_BRIGHT_BASE..CUSTOM_COLOR_FIELDS).contains(&i) => {
                self.bright[i - CUSTOM_BRIGHT_BASE] = value;
            }
            _ => {}
        }
    }

    /// 坏数据（手改 JSON / 空串 / 非十六进制）回落到默认 One Dark，避免终端渲染出透明块。
    fn clamp(&mut self) {
        let defaults = Self::default();
        for index in 0..CUSTOM_COLOR_FIELDS {
            let value = self.hex_field(index).to_string();
            let fixed =
                normalize_hex(&value).unwrap_or_else(|| defaults.hex_field(index).to_string());
            self.set_hex_field(index, fixed);
        }
    }

    /// 转成 rgpui-term 的配置；光标跟随前景色，明亮白兼作 bright 前景。
    fn to_config(&self) -> TerminalThemeConfig {
        let mut config = TerminalThemeConfig::default();
        config.foreground = self.foreground.clone();
        config.background = self.background.clone();
        config.cursor = self.foreground.clone();
        config.ansi = self.ansi.clone();
        config.bright = self.bright.clone();
        config.bright_foreground = Some(self.bright[7].clone());
        config.dim_foreground = Some(self.ansi[7].clone());
        config
    }
}

/// 把 `#RGB` / `#RRGGBB`（大小写、省略 `#` 均可）规范成 `#RRGGBB`；不合法返回 None。
fn normalize_hex(text: &str) -> Option<String> {
    let raw = text.trim().trim_start_matches('#');
    let expanded = match raw.len() {
        3 => raw.chars().flat_map(|ch| [ch, ch]).collect::<String>(),
        6 => raw.to_string(),
        _ => return None,
    };
    expanded
        .chars()
        .all(|ch| ch.is_ascii_hexdigit())
        .then(|| format!("#{}", expanded.to_ascii_uppercase()))
}

/// `#RRGGBB` → HSLA（用于色块预览；不合法返回 None，预览退回边框色）。
fn hex_to_hsla(hex: &str) -> Option<Hsla> {
    let normalized = normalize_hex(hex)?;
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&normalized[range], 16)
            .ok()
            .map(|value| value as f32 / 255.0)
    };
    Some(
        Rgba {
            r: channel(1..3)?,
            g: channel(3..5)?,
            b: channel(5..7)?,
            a: 1.0,
        }
        .into(),
    )
}

/// 终端外观偏好：字号、行高与配色方案（方案名取 `ThemeManager` 预设，或「自定义」）。
///
/// 只影响终端渲染，不动界面主题：终端有自己的一套 ANSI 配色，
/// 与 rgpui 全局 `Theme` 无关（后者管窗口 chrome）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalPrefs {
    /// 终端字号（px）。
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// 行高倍数。
    #[serde(default = "default_line_height")]
    pub line_height: f32,
    /// 配色方案名（`ThemeManager::available_themes()` 之一，含「自定义」）。
    #[serde(default = "default_terminal_theme")]
    pub theme: String,
    /// 自定义 16 色（选「自定义」时生效，始终随偏好一起持久化）。
    #[serde(default)]
    pub custom: TerminalColors,
}

fn default_font_size() -> f32 {
    FONT_SIZE_DEFAULT
}

fn default_line_height() -> f32 {
    LINE_HEIGHT_DEFAULT
}

fn default_terminal_theme() -> String {
    TERMINAL_THEME_DEFAULT.to_string()
}

impl Default for TerminalPrefs {
    fn default() -> Self {
        Self {
            font_size: FONT_SIZE_DEFAULT,
            line_height: LINE_HEIGHT_DEFAULT,
            theme: TERMINAL_THEME_DEFAULT.to_string(),
            custom: TerminalColors::default(),
        }
    }
}

impl TerminalPrefs {
    /// 把数值夹回合法区间：库里存的是历史版本写下的值，坏数据不该让终端不可读。
    fn clamp(&mut self) {
        self.font_size = self.font_size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX).round();
        self.line_height = self.line_height.clamp(LINE_HEIGHT_MIN, LINE_HEIGHT_MAX);
        if self.theme.is_empty() {
            self.theme = TERMINAL_THEME_DEFAULT.to_string();
        }
        self.custom.clamp();
    }

    /// 字号增减一档（`+1` / `-1`），结果仍夹在合法区间内。
    fn nudge_font_size(&mut self, steps: i32) {
        let next = self.font_size + FONT_SIZE_STEP * steps as f32;
        self.font_size = next;
        self.clamp();
    }

    /// 行高增减一档。
    fn nudge_line_height(&mut self, steps: i32) {
        let next = self.line_height + LINE_HEIGHT_STEP * steps as f32;
        self.line_height = next;
        self.clamp();
    }

    /// 序列化为入库/入文件的字符串。
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 解析库内 / settings.json 里的偏好；损坏或为空时回默认。
    fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        let mut prefs: Self = serde_json::from_str(text).unwrap_or_default();
        prefs.clamp();
        prefs
    }
}

/// 连接超时范围（秒）：太短会把慢服务器误判成不通，太长则白等。
const CONNECT_TIMEOUT_MIN: u64 = 5;
const CONNECT_TIMEOUT_MAX: u64 = 120;
const CONNECT_TIMEOUT_STEP: u64 = 5;
const CONNECT_TIMEOUT_DEFAULT: u64 = 15;
/// 自动重连最多连续尝试次数：断网时无限重试既吵又费资源，5 次后交给用户手动决定。
const AUTO_RECONNECT_MAX: u32 = 5;
/// 会话稳定存活超过这个时长后再次断线，重新计算自动重连次数（避免历史断线把额度吃光）。
const AUTO_RECONNECT_HEALTHY: Duration = Duration::from_secs(30);

/// 内嵌私钥文件大小上限：正常私钥几 KB，超过这个数多半是选错了文件。
const MAX_INLINE_KEY_BYTES: usize = 64 * 1024;

/// 第 n 次自动重连前的等待：退避避免对刚恢复的网络连打。
fn auto_reconnect_delay(attempt: u32) -> Duration {
    Duration::from_secs(match attempt {
        0 | 1 => 1,
        2 => 2,
        3 => 5,
        4 => 10,
        _ => 20,
    })
}

/// 会话行为偏好：断线自动重连与建连超时。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPrefs {
    /// 连接意外中断（网络断开、服务端掉线）后自动重连；远端主动 `exit` 不触发。
    #[serde(default = "auto_reconnect_default")]
    pub auto_reconnect: bool,
    /// 建连 + 认证 + 开 shell 的总超时（秒）。
    #[serde(default = "connect_timeout_default")]
    pub connect_timeout: u64,
}

fn auto_reconnect_default() -> bool {
    true
}

fn connect_timeout_default() -> u64 {
    CONNECT_TIMEOUT_DEFAULT
}

impl Default for SessionPrefs {
    fn default() -> Self {
        Self {
            auto_reconnect: true,
            connect_timeout: CONNECT_TIMEOUT_DEFAULT,
        }
    }
}

impl SessionPrefs {
    /// 超时夹回合法区间（库里可能是历史版本写下的越界值）。
    fn clamp(&mut self) {
        self.connect_timeout = self
            .connect_timeout
            .clamp(CONNECT_TIMEOUT_MIN, CONNECT_TIMEOUT_MAX);
    }

    /// 超时增减一档，结果仍夹在合法区间内。
    fn nudge_connect_timeout(&mut self, steps: i32) {
        let delta = CONNECT_TIMEOUT_STEP as i64 * i64::from(steps);
        let next = (self.connect_timeout as i64 + delta).max(0) as u64;
        self.connect_timeout = next;
        self.clamp();
    }

    /// 序列化为入库/入文件的字符串。
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 解析库内 / settings.json 里的偏好；损坏或为空时回默认。
    fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        let mut prefs: Self = serde_json::from_str(text).unwrap_or_default();
        prefs.clamp();
        prefs
    }
}

/// 一条保存下来的端口转发规则（WindTerm 式"先保存、再开关"）。
///
/// 只存规则本身，不存"当前是否在跑"——开关是会话内状态，重连后由
/// [`SavedForward::auto_start`] 决定要不要自动打开。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedForward {
    /// 备注名（可空，空时列表显示规则描述）。
    #[serde(default)]
    pub name: String,
    pub kind: session::ForwardKind,
    #[serde(default = "forward_listen_default")]
    pub listen_port: u16,
    #[serde(default = "forward_host_default")]
    pub target_host: String,
    #[serde(default = "forward_port_default")]
    pub target_port: u16,
    /// 连接建立后自动开启。
    #[serde(default)]
    pub auto_start: bool,
}

fn forward_listen_default() -> u16 {
    8080
}

fn forward_host_default() -> String {
    "127.0.0.1".to_string()
}

fn forward_port_default() -> u16 {
    80
}

impl SavedForward {
    /// 折算成会话层规则。
    pub fn rule(&self) -> session::ForwardRule {
        session::ForwardRule {
            kind: self.kind,
            listen_port: self.listen_port,
            target_host: self.target_host.clone(),
            target_port: self.target_port,
        }
    }

    /// 列表主标题：有备注用备注，否则用规则描述。
    fn title(&self) -> String {
        let name = self.name.trim();
        if name.is_empty() {
            self.rule().label()
        } else {
            name.to_string()
        }
    }
}

/// 端口转发偏好：按连接（`user@host:port`）保存规则列表。
///
/// 与侧边栏分组一样整体序列化成一个 JSON 串，存加密库 `settings` 表的
/// `forwards` 键（降级时进 settings.json），不动 `hosts` 表结构。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardPrefs {
    #[serde(default)]
    rules: HashMap<String, Vec<SavedForward>>,
}

impl ForwardPrefs {
    /// 某连接的规则列表（无记录时返回空切片）。
    fn rules_for(&self, addr: &str) -> &[SavedForward] {
        self.rules.get(addr).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 保存一条规则：同方向同监听端口视为同一条，原位替换，避免重复行。
    fn insert(&mut self, addr: &str, saved: SavedForward) {
        let id = saved.rule().id();
        let list = self.rules.entry(addr.to_string()).or_default();
        match list.iter_mut().find(|item| item.rule().id() == id) {
            // 覆盖已有规则时表单没填名字就沿用原名，避免"改个端口把备注弄丢"。
            Some(existing) => {
                let name = if saved.name.trim().is_empty() {
                    existing.name.clone()
                } else {
                    saved.name.clone()
                };
                *existing = SavedForward { name, ..saved };
            }
            None => list.push(saved),
        }
    }

    /// 删除某连接下的一条规则。
    fn remove(&mut self, addr: &str, rule_id: &str) {
        let Some(list) = self.rules.get_mut(addr) else {
            return;
        };
        list.retain(|item| item.rule().id() != rule_id);
        if list.is_empty() {
            self.rules.remove(addr);
        }
    }

    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 主机 → 跳板机映射（host id → jump host id）。
///
/// 为什么不进 hosts 表：加密库的表由 toasty 建模，给已有库加列需要手工 ALTER
/// 并处理旧库；和侧边栏分组一样放进 `settings` 表的一个 JSON 键即可，
/// 启动后回填到 [`HostConfig::proxy_jump`]。降级模式直接随 sessions.json 落盘。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JumpPrefs {
    /// 键是字符串形式的 host id（JSON 对象的键只能是字符串）。
    #[serde(default)]
    jumps: BTreeMap<String, i64>,
}

impl JumpPrefs {
    fn jump_of(&self, host_id: i64) -> Option<i64> {
        self.jumps.get(&host_id.to_string()).copied()
    }

    fn set(&mut self, host_id: i64, jump_id: Option<i64>) {
        let key = host_id.to_string();
        match jump_id {
            Some(id) => {
                self.jumps.insert(key, id);
            }
            None => {
                self.jumps.remove(&key);
            }
        }
    }

    /// 删除主机后清理引用：它作为目标、或作为别人的跳板机都要断掉。
    /// 返回被清掉跳板机的宿主 id，调用方据此改内存里的主机。
    fn forget(&mut self, host_id: i64) -> Vec<i64> {
        self.jumps.remove(&host_id.to_string());
        let affected: Vec<i64> = self
            .jumps
            .iter()
            .filter(|(_, jump)| **jump == host_id)
            .filter_map(|(host, _)| host.parse().ok())
            .collect();
        self.jumps.retain(|_, jump| *jump != host_id);
        affected
    }

    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 本机 shell 的偏好：自定义展示名 + 起始工作目录 + 启动参数 + 环境变量
/// （按 profile id = 路径 + 参数记账）。
///
/// 扫描结果每次重建，偏好单独存一份，重扫后仍生效；清空对应条目即恢复默认。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalShellPrefs {
    #[serde(default)]
    names: HashMap<String, String>,
    /// profile id → 起始工作目录（未设置则用进程当前目录）。
    #[serde(default)]
    cwd: HashMap<String, String>,
    /// profile id → 自定义启动参数原文（空格分隔，可带双引号）。
    #[serde(default)]
    args: HashMap<String, String>,
    /// profile id → 环境变量原文（每行 KEY=VALUE）。
    #[serde(default)]
    env: HashMap<String, String>,
}

impl LocalShellPrefs {
    /// 该 profile 的自定义名（空串视为未设置）。
    fn name_of(&self, id: &str) -> Option<&str> {
        self.names
            .get(id)
            .map(String::as_str)
            .filter(|name| !name.is_empty())
    }

    /// 设置别名；空名等于恢复默认（删除别名）。
    fn rename(&mut self, id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.names.remove(id);
        } else {
            self.names.insert(id.to_string(), name.to_string());
        }
    }

    /// 该 profile 的起始目录（未设置或空串视为默认）。
    fn cwd_of(&self, id: &str) -> Option<&str> {
        self.cwd
            .get(id)
            .map(String::as_str)
            .filter(|dir| !dir.is_empty())
    }

    /// 设置起始目录；空串等于恢复默认（删除该条）。
    fn set_cwd(&mut self, id: &str, dir: &str) {
        let dir = dir.trim();
        if dir.is_empty() {
            self.cwd.remove(id);
        } else {
            self.cwd.insert(id.to_string(), dir.to_string());
        }
    }

    /// 解析后的自定义启动参数（原始文本已在保存时校验并规范化）。
    fn args_of(&self, id: &str) -> Vec<String> {
        self.args
            .get(id)
            .map(|text| local_shell::split_args(text))
            .unwrap_or_default()
    }

    /// 保存自定义启动参数原文；空串等于不加参数。
    fn set_args(&mut self, id: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.args.remove(id);
        } else {
            self.args.insert(id.to_string(), text.to_string());
        }
    }

    /// 该 profile 的环境变量原文（未设置为空串）。
    fn env_of(&self, id: &str) -> &str {
        self.env.get(id).map(String::as_str).unwrap_or("")
    }

    /// 解析后的环境变量（保存时已校验，解析不会失败）。
    fn env_map_of(&self, id: &str) -> HashMap<String, String> {
        local_shell::parse_env(self.env_of(id)).unwrap_or_default()
    }

    /// 保存环境变量原文；空串等于不注入额外变量。
    fn set_env(&mut self, id: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.env.remove(id);
        } else {
            self.env.insert(id.to_string(), text.to_string());
        }
    }

    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 侧边栏分组（文件夹）偏好：组顺序 + 折叠态 + 主机归属。
///
/// 整体序列化成一个 JSON 串，存进加密库 `settings` 表的 `sidebar` 键（降级时进
/// settings.json）。刻意不给 `hosts` 表加列：加列要在加密库上做真实 ALTER，
/// 而分组只是展示层元数据，单独一个键即可，风险与迁移成本都低得多。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SidebarPrefs {
    /// 用户分组，按此顺序显示（未分组恒在末尾，不占位）。
    #[serde(default)]
    groups: Vec<GroupEntry>,
    /// 主机 id → 组名。组被删除后残留的映射按未分组处理，不报错。
    #[serde(default)]
    assign: HashMap<i64, String>,
    /// 「未分组」文件夹是否折叠。
    #[serde(default)]
    ungrouped_collapsed: bool,
    /// 主机 id 的手动排序（跨组一条序列，渲染时只取组内相对次序）。
    #[serde(default)]
    order: Vec<i64>,
}

/// 一个分组（文件夹）条目。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupEntry {
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
}

/// 未分组文件夹的名字（不作为 `groups` 条目存在）。
const UNGROUPED: &str = "未分组";

impl SidebarPrefs {
    /// 组名列表（不含未分组）。
    fn names(&self) -> impl Iterator<Item = &str> {
        self.groups.iter().map(|group| group.name.as_str())
    }

    /// 该主机所属分组；未分组或组已删除时返回 None。
    fn group_of(&self, id: i64) -> Option<&str> {
        let name = self.assign.get(&id).map(String::as_str)?;
        self.groups
            .iter()
            .any(|group| group.name == name)
            .then_some(name)
    }

    /// 新建分组；名字为空或已存在时返回 false。
    fn add(&mut self, name: &str) -> bool {
        let name = name.trim();
        !name.is_empty()
            && name != UNGROUPED
            && !self.groups.iter().any(|group| group.name == name)
            && {
                self.groups.push(GroupEntry {
                    name: name.to_string(),
                    collapsed: false,
                });
                true
            }
    }

    /// 重命名分组，并把组内主机的归属一并改写；目标名非法或原名不存在返回 false。
    fn rename(&mut self, from: &str, to: &str) -> bool {
        let to = to.trim();
        if to.is_empty()
            || to == UNGROUPED
            || (to != from && self.groups.iter().any(|g| g.name == to))
        {
            return false;
        }
        let Some(group) = self.groups.iter_mut().find(|group| group.name == from) else {
            return false;
        };
        group.name = to.to_string();
        for name in self.assign.values_mut() {
            if name == from {
                *name = to.to_string();
            }
        }
        true
    }

    /// 删除分组：组内主机退回未分组（只删归属，不删主机）。
    fn remove(&mut self, name: &str) {
        self.groups.retain(|group| group.name != name);
        self.assign.retain(|_, current| current != name);
    }

    /// 折叠 / 展开分组。
    fn toggle(&mut self, name: &str) {
        if name == UNGROUPED {
            self.ungrouped_collapsed = !self.ungrouped_collapsed;
            return;
        }
        if let Some(group) = self.groups.iter_mut().find(|group| group.name == name) {
            group.collapsed = !group.collapsed;
        }
    }

    /// 展开分组（新主机落进折叠组时调用，避免保存后"看不见"）。
    fn expand(&mut self, name: &str) {
        if name == UNGROUPED {
            self.ungrouped_collapsed = false;
            return;
        }
        if let Some(group) = self.groups.iter_mut().find(|group| group.name == name) {
            group.collapsed = false;
        }
    }

    fn collapsed(&self, name: &str) -> bool {
        if name == UNGROUPED {
            return self.ungrouped_collapsed;
        }
        self.groups
            .iter()
            .any(|group| group.name == name && group.collapsed)
    }

    /// 把主机移入某分组；`None` 表示未分组。
    fn assign_to(&mut self, id: i64, group: Option<&str>) {
        match group {
            Some(name) => {
                self.assign.insert(id, name.to_string());
            }
            None => {
                self.assign.remove(&id);
            }
        }
    }

    /// 主机删除后清理残留归属。
    fn forget(&mut self, id: i64) {
        self.assign.remove(&id);
        self.order.retain(|&item| item != id);
    }

    /// 主机在手动排序里的位置；未登记的主机排到已登记之后。
    fn rank_of(&self, id: i64) -> usize {
        self.order
            .iter()
            .position(|&item| item == id)
            .unwrap_or(usize::MAX)
    }

    /// 用当前存活主机补齐排序表：删掉的清出去，新主机按给定顺序追加到尾部。
    fn sync_order(&mut self, alive: &[i64]) {
        self.order.retain(|id| alive.contains(id));
        let mut seen = self.order.clone();
        for &id in alive {
            if !seen.contains(&id) {
                seen.push(id);
                self.order.push(id);
            }
        }
    }

    /// 把 `id` 移到 `anchor` 之前（锚点不存在时追加到尾部）。
    fn move_before(&mut self, id: i64, anchor: i64) {
        if id == anchor {
            return;
        }
        self.order.retain(|&item| item != id);
        match self.order.iter().position(|&item| item == anchor) {
            Some(position) => self.order.insert(position, id),
            None => self.order.push(id),
        }
    }

    /// 序列化为入库/入文件的字符串。
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 解析库内 / settings.json 里的分组偏好；损坏或为空时回默认。
    fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 侧边栏右键菜单的目标。
#[derive(Clone)]
enum MenuTarget {
    /// 列表空白处：只给新建类入口。
    Blank,
    /// 某个分组（文件夹）：附带重命名 / 删除。
    Group(SharedString),
    /// 某台主机行（真实下标）：附带编辑、移动分组、删除。
    Host(usize),
}

/// 组内手动排序动作（收藏 / 非收藏各自成桶，不互相穿插）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReorderAction {
    Up,
    Down,
    Top,
}

/// RuLink 主视图：左侧主机账号管理，右侧会话标签终端。
struct RuLinkApp {
    /// 会话仓库（负责持久化）。
    store: SessionStore,
    /// 存储模式（便携 / 全局），启动时探测一次。
    mode: StorageMode,
    /// 加密凭据库；None 表示启动引导中、已降级为旧 JSON 存储，或被用户锁定。
    vault: Option<Arc<Vault>>,
    /// 本次解锁所用的库主密钥（十六进制）。只存活在内存，绝不入库、入日志、入命令行；
    /// 换主密码前用它校验"当前主密码"，锁定/退出即丢弃。
    unlocked_key: Option<String>,
    /// 已保存的主机账号列表。
    hosts: Vec<HostConfig>,
    /// 最近成功连接的加密库主机 id（新的在前），供「会话」菜单快速重连。
    recent_hosts: Vec<i64>,
    /// 主机列表搜索框（按展示名 / user@host:port 子串过滤）。
    search: Entity<InputState>,
    /// 活动会话标签。
    tabs: Vec<SessionTab>,
    /// 当前选中标签 id。
    active_tab: Option<usize>,
    /// 打开的新建主机表单（None 显示列表）。
    new_host: Option<NewHostForm>,
    /// 等待密码输入的连接。
    pending: Option<PendingConnect>,
    /// 本机 shell 档案列表：启动时 `detect()`（只查文件），点「一键扫描」后含 WSL。
    local_shells: Vec<ShellProfile>,
    /// 可执行文件路径 → 系统默认图标（PNG 解码后的 `Image`），扫描后填充。
    shell_icons: HashMap<String, Arc<Image>>,
    /// 扫描进行中：按钮禁用 + 状态栏提示，避免连点起多个 wsl.exe。
    scanning_shells: bool,
    /// 「本地终端」分组的折叠状态。只影响这次会话——它不是库里的分组，没必要占持久化键。
    local_group_collapsed: bool,
    /// 本机 shell 的自定义展示名，随加密库 / settings.json 双层持久化。
    local_shell_prefs: LocalShellPrefs,
    /// 本地 shell 重命名弹窗。
    shell_prompt: Option<ShellPrompt>,
    /// 标签栏横向滚动句柄：新建 / 切到标签时把它滚进可视区。
    tab_scroll: ScrollHandle,
    /// 侧边栏分组（文件夹）偏好，随加密库 / settings.json 双层持久化。
    sidebar: SidebarPrefs,
    /// 主机 → 跳板机映射（host id → jump id），双层持久化。
    jumps: JumpPrefs,
    /// 分组新建 / 重命名弹窗（`renaming` 为 None 表示新建）。
    group_prompt: Option<GroupPrompt>,
    /// 删除二次确认（真实下标；浮层遮挡期间列表不会变）。
    confirm_delete: Option<usize>,
    /// 主密码解锁弹窗（便携模式 / 已设置主密码时启动弹出）。
    unlock: Option<UnlockState>,
    /// 设置弹窗（主密码管理、主题、锁定）。
    settings: Option<SettingsState>,
    /// 文件传输弹窗（上传 / 下载）。
    transfer: Option<TransferDialog>,
    /// 端口转发弹窗。
    forward: Option<ForwardDialog>,
    /// 本次运行内缓存的密码（按 user@host:port），避免重复弹窗；绝不落盘。
    password_cache: HashMap<String, String>,
    /// 内嵌私钥（host_id → PEM 明文）：开库时整体读入内存，连接时直接取用。
    inline_keys: HashMap<i64, String>,
    /// 标签自增 id。
    next_id: usize,
    /// 状态栏左侧的瞬态消息（连接/保存/删除/断开等节点更新）。
    status: SharedString,
    /// 状态栏显示项偏好（勾选哪些条目），随加密库 / settings.json 持久化。
    status_bar: StatusBarPrefs,
    /// 终端外观偏好（字号 / 行高 / 配色方案 / 自定义 16 色），同样双层持久化。
    terminal: TerminalPrefs,
    /// 自定义配色的 18 个十六进制输入框（前景 / 背景 / 普通 8 色 / 明亮 8 色）。
    custom_color_inputs: Vec<Entity<InputState>>,
    /// 会话行为偏好（自动重连 / 连接超时），同样双层持久化。
    session_prefs: SessionPrefs,
    /// 端口转发规则（按连接保存，开关控制启停），同样双层持久化。
    forward_prefs: ForwardPrefs,
    /// 侧边栏 ↔ 主区 的分隔面板状态（拖拽调整宽度 + 折叠/展开）。
    split_state: Entity<SplitPaneState>,
    /// 主区分屏（两个标签并排）；None＝单屏显示活动标签。
    view_split: Option<SplitView>,
    focus_handle: FocusHandle,
}

/// 主题调色板：从 rgpui 全局 `Theme` 的语义色映射而来（不硬编码颜色）。
/// 切换明暗只调 `Theme::change`，本结构随之重取，界面整体换色，且与按钮 / 状态栏等
/// 组件共用同一份主题（避免手工配色与框架主题脱节）。
struct Palette {
    /// 窗口/编辑区底色。
    window_bg: Hsla,
    /// 侧边栏与标签栏底色。
    sidebar_bg: Hsla,
    /// 卡片/表单/非活动标签底色。
    card_bg: Hsla,
    /// 卡片悬停底色。
    card_hover: Hsla,
    /// 分隔边框色。
    border: Hsla,
    /// 主文字色。
    text: Hsla,
    /// 次要/弱化文字色。
    muted: Hsla,
    /// 认证方式链接色。
    link: Hsla,
    /// 删除/关闭等危险操作色。
    danger: Hsla,
    /// 选中/主按钮强调色。
    accent: Hsla,
    /// 未选中分段（认证方式切换）底色。
    segment_bg: Hsla,
    /// 输入框底色。
    input_bg: Hsla,
    /// 输入框文字色。
    input_text: Hsla,
    /// 输入框边框色。
    input_border: Hsla,
    /// 标签栏底色（与编辑区、非活动标签区分）。
    tab_bar_bg: Hsla,
    /// 活动标签底色。
    tab_active_bg: Hsla,
    /// 活动标签文字色。
    tab_active_fg: Hsla,
    /// 非活动标签文字色。
    tab_fg: Hsla,
}

impl Palette {
    /// 从当前全局主题取语义色，组装一次性调色板。
    fn from_theme(cx: &App) -> Self {
        let theme = cx.theme();
        Palette {
            window_bg: theme.background,
            sidebar_bg: theme.sidebar,
            card_bg: theme.popover,
            card_hover: theme.list_hover,
            border: theme.border,
            text: theme.foreground,
            muted: theme.muted_foreground,
            link: theme.link,
            danger: theme.danger,
            accent: theme.primary,
            segment_bg: theme.muted,
            input_bg: theme.input_background(),
            input_text: theme.foreground,
            input_border: theme.border,
            tab_bar_bg: theme.tab_bar,
            tab_active_bg: theme.tab_active,
            tab_active_fg: theme.tab_active_foreground,
            tab_fg: theme.tab_foreground,
        }
    }
}

impl RuLinkApp {
    /// 载入已保存主机，可选执行命令行快速连接。
    fn new(quick: Option<SshTarget>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mode = StorageMode::detect();
        let store = SessionStore::new(&mode);
        let hosts = store.load();
        // 开库前先用旧 settings.json 里的状态栏偏好，库打开后再被库内值覆盖。
        let settings = store.load_settings();
        let status_bar = StatusBarPrefs::from_json(&settings.status_bar);
        let terminal = TerminalPrefs::from_json(&settings.terminal);
        let session_prefs = SessionPrefs::from_json(&settings.session);
        let local_shell_prefs = LocalShellPrefs::from_json(&settings.local_shells);
        let sidebar = SidebarPrefs::from_json(&settings.sidebar);
        let forward_prefs = ForwardPrefs::from_json(&settings.forwards);
        let jumps = JumpPrefs::from_json(&settings.jumps);
        // 终端配色预设管理器：全局一份，`TerminalView::set_theme` 与自建样式都从这里取。
        if !cx.has_global::<ThemeManager>() {
            cx.set_global(ThemeManager::new(None));
        }
        // 自定义 16 色作为一套叫「自定义」的方案注册进管理器，选择器与渲染共用同一份。
        Self::register_custom_theme(&terminal.custom, cx);
        // 18 个十六进制输入框常驻（设置弹窗反复打开也不用重建），
        // 编辑时立即刷新色块与已开终端；程序化回填走 set_value，不会触发提交。
        let custom_color_inputs = (0..CUSTOM_COLOR_FIELDS)
            .map(|index| {
                let value = terminal.custom.hex_field(index);
                let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
                cx.subscribe(&input, move |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.commit_custom_color(index, cx);
                    }
                })
                .detach();
                input
            })
            .collect::<Vec<_>>();
        // 侧边栏 ↔ 主区 分隔面板：初始侧边栏占比约 0.25。
        let split_state = cx.new(SplitPaneState::new);
        split_state.update(cx, |state, cx| {
            state.set_ratio(0.25, cx);
            // WindTerm 式拖动：拖到最小宽度即停住，分隔线仍可抓住往大拖；
            // 完全收起交给标题栏 « 折叠按钮 / Ctrl+B，不靠拖拽阈值自动折叠。
            state.set_min_first(160.0, cx);
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索主机…"));
        cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        let mut app = Self {
            store,
            mode,
            vault: None,
            hosts,
            recent_hosts: Vec::new(),
            search,
            tabs: Vec::new(),
            active_tab: None,
            new_host: None,
            pending: None,
            local_shells: local_shell::detect(),
            shell_icons: HashMap::new(),
            scanning_shells: false,
            local_group_collapsed: false,
            local_shell_prefs,
            shell_prompt: None,
            tab_scroll: ScrollHandle::new(),
            sidebar,
            jumps,
            group_prompt: None,
            confirm_delete: None,
            unlock: None,
            settings: None,
            transfer: None,
            forward: None,
            unlocked_key: None,
            password_cache: HashMap::new(),
            inline_keys: HashMap::new(),
            next_id: 0,
            status: "就绪".into(),
            status_bar,
            terminal,
            custom_color_inputs,
            session_prefs,
            forward_prefs,
            split_state,
            view_split: None,
            focus_handle: cx.focus_handle(),
        };
        app.boot_vault(window, cx);
        // 启动档 shell 只有几个，图标走 shell 缓存、毫秒级，直接在主线程取好再进列表。
        app.refresh_shell_icons();
        app.auto_scan_local_shells(cx);
        if let Some(target) = quick {
            let title = target.addr();
            app.connect(title, target, None, None, false, window, cx);
        }
        app
    }

    /// 异步打开加密凭据库：成功则接管主机列表（含一次性迁移旧库/JSON 并改名保留），
    /// 失败则降级回旧 JSON 存储并提示（降级模式下密码只留在内存）。
    ///
    /// 口令档（已设置主密码）启动时弹出解锁对话框，口令错误可重试；
    /// keyring 档保持零输入直连。
    fn boot_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self.mode.clone();
        let json_hosts = self.hosts.clone();
        // 凭据库（toasty/turso）必须跑在 tokio 运行时上，句柄在此处取出后交给任务。
        let runtime = rgpui::tokio::Tokio::handle(cx);
        // 只有口令档才弹解锁框：便携模式不再强制设密码，默认可免密使用，
        // 是否配置主密码交由设置界面决定。
        let passphrase_needed =
            crate::vault::read_unlock_marker(&mode) == crate::vault::UnlockBackend::Passphrase;
        if passphrase_needed {
            self.open_unlock_dialog(window, cx);
        } else if mode.is_portable() {
            // 便携库若仍由本机凭据管理器加密，文件拷到其他机器无法解锁：只提示不拦路。
            self.status =
                "便携模式正用本机凭据管理器解锁；建议到设置中配置主密码，以便带往其他机器".into();
        }
        cx.spawn(async move |this, cx| {
            // 口令档（已设置主密码）：密钥只能来自用户输入，
            // 绝不能用 keyring 随机密钥代开——那会让主密码形同虚设。
            if passphrase_needed {
                return;
            }
            // 库文件在数据目录还是原样，先探测主密钥是否可得。
            let keyring_key = match crate::vault::load_or_create_master_key() {
                Ok(key) => Some(key),
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        log::warn!("系统凭据管理器不可用: {error}");
                        this.status =
                            SharedString::from(format!("凭据管理器不可用（{error}），不保存密码"));
                        cx.notify();
                    });
                    return;
                }
            };
            let opened = async {
                let vault =
                    Vault::open(&mode, keyring_key.clone().unwrap(), runtime.clone()).await?;
                let hosts =
                    Self::after_vault_open(&vault, &mode, keyring_key.as_deref(), &json_hosts)
                        .await?;
                Ok::<_, anyhow::Error>((vault, hosts, keyring_key.unwrap()))
            }
            .await;
            match opened {
                Ok((vault, hosts, key)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.attach_vault(vault, hosts, Some(key), cx);
                    });
                }
                Err(error) => {
                    log::warn!("加密凭据库不可用: {error}");
                    let _ = this.update(cx, |this, cx| {
                        if crate::vault::read_unlock_marker(&this.mode)
                            == crate::vault::UnlockBackend::Passphrase
                        {
                            // 口令档打不开：交回解锁弹窗处理，不降级 JSON。
                            cx.notify();
                        } else {
                            this.status = SharedString::from(format!(
                                "加密凭据库不可用（{error}），沿用旧存储且不保存密码"
                            ));
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
    }

    /// 开库成功后的统一接管：以库内列表为准接管界面主机、补发启动期间新增项、
    /// 标记 JSON 迁移、应用库内主题。
    ///
    /// `db_hosts` 必须来自凭据库本身——只靠旧 JSON 的界面列表会在数据入库后
    /// 显示为空（设置主密码后"主机消失"即由此而来，数据其实没丢）。
    fn attach_vault(
        &mut self,
        vault: Vault,
        mut db_hosts: Vec<HostConfig>,
        key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let vault = Arc::new(vault);
        self.unlock = None;
        // 开库完成前用户新增的主机（id==0，彼时尚无法入库）：补发 id 并入库。
        let mut next = db_hosts.iter().map(|host| host.id).max().unwrap_or(0) + 1;
        for host in self.hosts.iter().filter(|host| host.id == 0) {
            let mut host = host.clone();
            host.id = next;
            next += 1;
            db_hosts.push(host.clone());
            let vault = vault.clone();
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.upsert(&host).await {
                    log::warn!("凭据库写入失败: {error}");
                }
            })
            .detach();
        }
        self.store.mark_migrated();
        self.hosts = db_hosts;
        self.vault = Some(vault.clone());
        self.unlocked_key = key;
        cx.notify();
        // 内嵌私钥整体读入内存：连接与断线重连都走这份缓存，不再访问库。
        let keys_vault = vault.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(keys) = keys_vault.load_host_keys().await {
                let _ = this.update(cx, |this, cx| {
                    this.inline_keys = keys.into_iter().collect();
                    cx.notify();
                });
            }
        })
        .detach();
        // 库内主题优先于启动时的旧 JSON/默认值。
        cx.spawn(async move |this, cx| {
            if let Ok(Some(theme)) = vault.setting("theme").await {
                let dark = theme != "light";
                let _ = this.update(cx, |_, cx| {
                    rgpui::theme::Theme::change(
                        if dark {
                            rgpui::theme::ThemeMode::Dark
                        } else {
                            rgpui::theme::ThemeMode::Light
                        },
                        None,
                        cx,
                    );
                    cx.notify();
                });
            }
            // 状态栏显示项同样以库内为准。
            if let Ok(Some(json)) = vault.setting("status_bar").await {
                let prefs = StatusBarPrefs::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.status_bar != prefs {
                        this.status_bar = prefs;
                        cx.notify();
                    }
                });
            }
            // 终端外观（字号 / 行高 / 配色）同样以库内为准，此时还没有会话，无需重绘。
            if let Ok(Some(json)) = vault.setting("terminal").await {
                let prefs = TerminalPrefs::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    // 自定义 16 色可能刚被库内值替换，重新注册后再按新配色渲染。
                    Self::register_custom_theme(&prefs.custom, cx);
                    if this.terminal != prefs {
                        this.terminal = prefs;
                        cx.notify();
                    }
                });
            }
            // 会话行为（自动重连 / 连接超时）同样以库内为准。
            if let Ok(Some(json)) = vault.setting("session").await {
                let prefs = SessionPrefs::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.session_prefs != prefs {
                        this.session_prefs = prefs;
                        cx.notify();
                    }
                });
            }
            // 侧边栏分组同样以库内为准。
            if let Ok(Some(json)) = vault.setting("sidebar").await {
                let prefs = SidebarPrefs::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.sidebar != prefs {
                        this.sidebar = prefs;
                        cx.notify();
                    }
                });
            }
            // 最近连接：只保留仍存在的主机，顺序即最近先后。
            if let Ok(Some(json)) = vault.setting("recent").await
                && let Ok(mut ids) = serde_json::from_str::<Vec<i64>>(&json)
            {
                let _ = this.update(cx, |this, cx| {
                    ids.retain(|id| this.hosts.iter().any(|host| host.id == *id));
                    ids.truncate(RECENT_HOSTS_MAX);
                    if this.recent_hosts != ids {
                        this.recent_hosts = ids;
                        cx.notify();
                    }
                });
            }
            // 本机 shell 自定义名同样以库内为准（扫描结果不变，只需重绘列表）。
            if let Ok(Some(json)) = vault.setting("local_shells").await {
                let prefs = LocalShellPrefs::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.local_shell_prefs != prefs {
                        this.local_shell_prefs = prefs;
                        cx.notify();
                    }
                });
            }
            // 保存的端口转发规则同样以库内为准。
            if let Ok(Some(json)) = vault.setting("forwards").await {
                let prefs = ForwardPrefs::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.forward_prefs != prefs {
                        this.forward_prefs = prefs;
                        cx.notify();
                    }
                });
            }
            // 跳板机映射同样以库内为准：回填到已加载的主机上。
            if let Ok(Some(json)) = vault.setting("jumps").await {
                let prefs = JumpPrefs::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.jumps != prefs {
                        this.jumps = prefs;
                        this.apply_jumps();
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// 打开解锁弹窗（启动自动弹出；"退出应用"关闭后可经侧边栏 🔑 重新打开）。
    fn open_unlock_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.unlock.is_some() {
            return;
        }
        let passphrase = cx.new(|cx| InputState::new(window, cx).placeholder("主密码"));
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder("再次输入确认"));
        // 回车即解锁：两个输入框都订阅提交事件，无需再点按钮。
        for input in [&passphrase, &confirm] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.unlock_passphrase(cx);
                }
            })
            .detach();
        }
        passphrase.update(cx, |state, cx| state.focus(window, cx));
        self.unlock = Some(UnlockState {
            passphrase,
            confirm,
            is_new: !self.mode.db_path().exists(),
            error: None,
        });
        cx.notify();
    }

    /// 打开口令档加密库并完成一次性迁移（解锁弹窗确认后调用）。
    fn unlock_passphrase(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.unlock.as_ref() else {
            return;
        };
        let mut ctx = &mut *cx;
        let password = state.passphrase.read(&mut ctx).text().to_string();
        if state.is_new {
            let confirm = state.confirm.read(&mut ctx).text().to_string();
            if confirm != password {
                if let Some(state) = self.unlock.as_mut() {
                    state.error = Some("两次输入的主密码不一致".into());
                }
                cx.notify();
                return;
            }
        }
        if password.is_empty() {
            if let Some(state) = self.unlock.as_mut() {
                state.error = Some("请输入主密码".into());
            }
            cx.notify();
            return;
        }
        let mode = self.mode.clone();
        let json_hosts = self.hosts.clone();
        let salt_path = mode.salt_path();
        let is_new = self.unlock.as_ref().map(|s| s.is_new).unwrap_or(false);
        let runtime = rgpui::tokio::Tokio::handle(cx);
        cx.spawn(async move |this, cx| {
            let opened = async {
                // 盐缺失时生成并写盘（首次建库）；已有盐沿用（解锁）。
                let salt = crate::vault::load_or_create_salt(&salt_path)?;
                let hexkey = crate::vault::derive_key(&salt, &password)?;
                let vault = Vault::open(&mode, hexkey.clone(), runtime.clone()).await?;
                let keyring_key = crate::vault::load_or_create_master_key().ok();
                let hosts =
                    Self::after_vault_open(&vault, &mode, keyring_key.as_deref(), &json_hosts)
                        .await?;
                if is_new {
                    crate::vault::write_unlock_marker(
                        &mode,
                        crate::vault::UnlockBackend::Passphrase,
                    )?;
                }
                Ok::<_, anyhow::Error>((vault, hosts, hexkey))
            }
            .await;
            let _ = this.update(cx, |this, cx| match opened {
                Ok((vault, hosts, hexkey)) => {
                    this.attach_vault(vault, hosts, Some(hexkey), cx);
                }
                Err(error) => {
                    if let Some(state) = this.unlock.as_mut() {
                        state.error = Some(if is_new {
                            format!("创建加密库失败：{error}").into()
                        } else {
                            "主密码不正确，请重试（多次输入错误可退出）".into()
                        });
                        log::warn!("解锁加密库失败: {error}");
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 退出解锁弹窗（尚未解锁时保持降级 JSON 行为）。
    fn dismiss_unlock(&mut self, cx: &mut Context<Self>) {
        self.unlock = None;
        self.status = "凭据库未解锁，本次运行不保存密码".into();
        cx.notify();
    }

    /// 打开设置弹窗：主密码管理与主题偏好（主密码相关操作集中在设置里，
    /// 侧边栏只留入口）。
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_at(SETTINGS_TAB_VAULT, window, cx);
    }

    /// 打开设置弹窗并直接停在指定分页（标题栏菜单按入口意图分页）。
    fn open_settings_at(&mut self, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        // 库内终端偏好可能在上次打开之后才到达，先把自定义色回填到输入框。
        self.sync_custom_color_inputs(window, cx);
        if let Some(state) = self.settings.as_mut() {
            // 已开着：只换分页（搬迁途中锁住分页，避免切走看不到结果）。
            if !state.busy {
                state.tab = tab;
                cx.notify();
            }
            return;
        }
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        // 已是口令档且处于解锁状态：换密钥必须先证明知道当前主密码。
        let needs_current = passphrase_mode && self.vault.is_some();
        let current = needs_current
            .then(|| cx.new(|cx| InputState::new(window, cx).placeholder("当前主密码")));
        let password = cx.new(|cx| {
            InputState::new(window, cx).placeholder(password_placeholder(passphrase_mode))
        });
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder("再次输入确认"));
        // 任一输入框回车即保存，不必再点按钮。
        for input in [&password, &confirm].into_iter().chain(current.iter()) {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.apply_rekey(cx);
                }
            })
            .detach();
        }
        current
            .as_ref()
            .unwrap_or(&password)
            .update(cx, |state, cx| state.focus(window, cx));
        self.settings = Some(SettingsState {
            tab,
            current,
            password: Some(password),
            confirm: Some(confirm),
            error: None,
            info: None,
            busy: false,
        });
        cx.notify();
    }

    /// 切换设置弹窗分页；主密码搬迁途中锁住分页，免得用户切走看不到结果。
    fn select_settings_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(state) = self.settings.as_mut() else {
            return;
        };
        if state.busy || state.tab == ix {
            return;
        }
        state.tab = ix;
        cx.notify();
    }

    /// 重建口令表单（成功回显后输入框已销毁，"更改主密码"再次开启）。
    fn reopen_password_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .settings
            .as_ref()
            .is_some_and(|state| state.password.is_some())
        {
            return;
        }
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        let current = passphrase_mode
            .then(|| cx.new(|cx| InputState::new(window, cx).placeholder("当前主密码")));
        let password = cx.new(|cx| {
            InputState::new(window, cx).placeholder(password_placeholder(passphrase_mode))
        });
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder("再次输入确认"));
        for input in [&password, &confirm].into_iter().chain(current.iter()) {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.apply_rekey(cx);
                }
            })
            .detach();
        }
        let focus = current.as_ref().unwrap_or(&password).clone();
        if let Some(state) = self.settings.as_mut() {
            state.error = None;
            state.info = None;
            state.current = current;
            state.password = Some(password);
            state.confirm = Some(confirm);
        }
        focus.update(cx, |field, cx| field.focus(window, cx));
        cx.notify();
    }

    /// 关闭设置弹窗（搬迁进行中不许关闭，避免半途中止留下半套文件）。
    fn close_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.as_ref().is_some_and(|state| state.busy) {
            return;
        }
        self.settings = None;
        cx.notify();
    }

    /// Ctrl+Shift+U / 会话菜单：为当前标签打开文件传输弹窗。
    fn open_transfer_action(
        &mut self,
        _: &OpenTransfer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_transfer(window, cx);
    }

    /// 打开传输弹窗：借用当前 SSH 会话的 SFTP 子系统，不新开连接、不再认证。
    fn open_transfer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.transfer.is_some() || self.overlay_open() {
            return;
        }
        let Some(id) = self.active_tab else {
            self.status = "没有可传输的会话，请先连接 SSH 主机".into();
            cx.notify();
            return;
        };
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        if tab.ended {
            self.status = "会话已断开，请先重新连接再传输文件".into();
            cx.notify();
            return;
        }
        let Some(sender) = tab.commands.clone() else {
            self.status = "本地终端不支持文件传输，请连接 SSH 主机".into();
            cx.notify();
            return;
        };
        let title = tab.title.clone();
        let local = cx.new(|cx| InputState::new(window, cx).placeholder("本地文件路径"));
        let remote = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("~")
                .placeholder("远端路径，如 /var/tmp/app.log")
        });
        local.update(cx, |state, cx| state.focus(window, cx));
        self.transfer = Some(TransferDialog {
            tab_id: id,
            title,
            local: local.clone(),
            remote,
            home: None,
            busy: false,
            direction: None,
            progress: None,
            error: None,
            info: None,
        });
        // 主目录只是预填用：查不到也让用户手动输入路径，不挡住弹窗。
        let (reply, rx) = tokio::sync::oneshot::channel();
        if sender.send(session::SessionCommand::Home { reply }).is_ok() {
            let entity = cx.entity();
            let entity_id = entity.entity_id();
            cx.spawn(async move |_, cx| {
                let Ok(result) = rx.await else {
                    return;
                };
                let _ = cx.with_window(entity_id, move |window, cx| {
                    let _ = entity.update(cx, move |this, cx| {
                        this.apply_transfer_home(local, result, window, cx);
                    });
                });
            })
            .detach();
        }
        self.status = "文件传输：选择本地文件上传，或输入远端文件下载".into();
        cx.notify();
    }

    /// SFTP 主目录返回：预填远端路径（仅在用户尚未改写时），失败则提示手动输入。
    fn apply_transfer_home(
        &mut self,
        identity: Entity<InputState>,
        result: anyhow::Result<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self
            .transfer
            .as_mut()
            .filter(|dialog| dialog.local == identity)
        else {
            return;
        };
        match result {
            Ok(home) => {
                dialog.home = Some(home.clone());
                let remote = dialog.remote.clone();
                let current = remote.read(cx).value();
                let untouched = matches!(current.trim(), "" | "~" | "~/");
                if untouched {
                    remote.update(cx, |state, cx| state.set_value(home, window, cx));
                }
            }
            Err(error) => {
                dialog.info = Some(
                    format!(
                        "无法读取远端主目录（{}），请手动输入完整路径",
                        session::error_summary(&error)
                    )
                    .into(),
                );
            }
        }
        cx.notify();
    }

    /// 关闭传输弹窗；进行中的任务继续跑完（结果仍写状态栏），只是不再展示进度。
    fn close_transfer(&mut self, cx: &mut Context<Self>) {
        self.transfer = None;
        cx.notify();
    }

    /// 选择要上传的本地文件，并顺带把远端目标预填成「主目录/文件名」。
    fn browse_transfer_local(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.transfer.as_ref() else {
            return;
        };
        let input = dialog.local.clone();
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择要上传的文件".into()),
        });
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.first().cloned() else {
                return;
            };
            let value = path.to_string_lossy().into_owned();
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            let _ = cx.with_window(entity_id, move |window, cx| {
                let _ = entity.update(cx, move |this, cx| {
                    if this.transfer.as_ref().is_some_and(|d| d.local == input) {
                        input.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
                    }
                    let Some(dialog) = this.transfer.as_ref().filter(|d| d.local == input) else {
                        return;
                    };
                    let current = dialog.remote.read(cx).value();
                    let Some(home) = dialog.home.clone() else {
                        cx.notify();
                        return;
                    };
                    let Some(name) = name else {
                        cx.notify();
                        return;
                    };
                    // 用户没改过远端路径时，直接给出「主目录/文件名」，上传即可落地。
                    if matches!(current.trim(), "" | "~" | "~/") {
                        let target = format!("{}/{}", home.trim_end_matches('/'), name);
                        dialog
                            .remote
                            .update(cx, |state, cx| state.set_value(target, window, cx));
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 上传：本地路径 + 远端目标（目标为目录时由 transfer 层补上文件名）。
    fn upload_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.transfer.as_ref() else {
            return;
        };
        if dialog.busy {
            return;
        }
        let local = dialog.local.read(cx).value().trim().to_string();
        let remote = dialog.remote.read(cx).value().trim().to_string();
        if local.is_empty() {
            self.transfer_error("请先选择要上传的本地文件", cx);
            return;
        }
        if remote.is_empty() {
            self.transfer_error("请先输入远端目标路径", cx);
            return;
        }
        self.start_transfer(
            transfer::Direction::Upload,
            PathBuf::from(local),
            remote,
            cx,
        );
    }

    /// 下载：先让用户选保存位置，再按远端文件开始传输。
    fn download_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.transfer.as_ref() else {
            return;
        };
        if dialog.busy {
            return;
        }
        let remote = dialog.remote.read(cx).value().trim().to_string();
        if remote.is_empty() {
            self.transfer_error("请先输入要下载的远端文件路径", cx);
            return;
        }
        let name = Path::new(&remote)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".to_string());
        let start = downloads_dir();
        let input = dialog.local.clone();
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        let rx = cx.prompt_for_new_path(&start, Some(&name));
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let _ = cx.with_window(entity_id, move |window, cx| {
                let _ = entity.update(cx, move |this, cx| {
                    if !this.transfer.as_ref().is_some_and(|d| d.local == input) {
                        return;
                    }
                    let value = path.to_string_lossy().into_owned();
                    input.update(cx, |state, cx| state.set_value(value, window, cx));
                    this.start_transfer(transfer::Direction::Download, path, remote, cx);
                });
            });
        })
        .detach();
    }

    /// 发起一次传输：弹窗置忙，后台任务把进度与最终结果写回 UI。
    fn start_transfer(
        &mut self,
        direction: transfer::Direction,
        local: PathBuf,
        remote: String,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.transfer.as_ref() else {
            return;
        };
        if dialog.busy {
            return;
        }
        let tab_id = dialog.tab_id;
        let identity = dialog.local.clone();
        let Some(sender) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.commands.clone())
        else {
            self.transfer_error("会话已断开，无法传输文件", cx);
            return;
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, u64)>();
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        let command = session::SessionCommand::Transfer(transfer::Task {
            direction,
            local,
            remote,
            progress: progress_tx,
            reply: reply_tx,
        });
        if sender.send(command).is_err() {
            self.transfer_error("会话已断开，无法传输文件", cx);
            return;
        }
        if let Some(dialog) = self.transfer.as_mut() {
            dialog.busy = true;
            dialog.direction = Some(direction);
            dialog.progress = Some((0, 0));
            dialog.error = None;
            dialog.info = None;
        }
        self.status = SharedString::from(format!("正在{}文件…", direction.label()));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = loop {
                tokio::select! {
                    result = &mut reply_rx => {
                        break match result {
                            Ok(Ok(summary)) => Ok(summary),
                            Ok(Err(error)) => Err(session::error_summary(&error)),
                            Err(_) => Err("传输任务已中断".to_string()),
                        };
                    }
                    Some(progress) = progress_rx.recv() => {
                        let _ = this.update(cx, |this, cx| {
                            let Some(dialog) = this
                                .transfer
                                .as_mut()
                                .filter(|dialog| dialog.local == identity)
                            else {
                                return;
                            };
                            if dialog.busy {
                                dialog.progress = Some(progress);
                                cx.notify();
                            }
                        });
                    }
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.finish_transfer(identity, direction, outcome, cx);
            });
        })
        .detach();
    }

    /// 传输收尾：按钮解禁、写入结果文案（弹窗已关闭时只更新状态栏）。
    fn finish_transfer(
        &mut self,
        identity: Entity<InputState>,
        direction: transfer::Direction,
        outcome: Result<transfer::Summary, String>,
        cx: &mut Context<Self>,
    ) {
        let active = self
            .transfer
            .as_ref()
            .is_some_and(|dialog| dialog.local == identity);
        match outcome {
            Ok(summary) => {
                let message = format!(
                    "{}完成 · {} · {:.1} 秒",
                    direction.label(),
                    transfer::format_bytes(summary.bytes),
                    summary.elapsed.as_secs_f64()
                );
                if active && let Some(dialog) = self.transfer.as_mut() {
                    dialog.busy = false;
                    dialog.info = Some(message.clone().into());
                    dialog.error = None;
                    dialog.progress = Some((summary.bytes, summary.bytes));
                }
                self.status = message.into();
            }
            Err(error) => {
                if active && let Some(dialog) = self.transfer.as_mut() {
                    dialog.busy = false;
                    dialog.info = None;
                    dialog.error = Some(error.clone().into());
                }
                self.status = format!("{}失败：{error}", direction.label()).into();
            }
        }
        cx.notify();
    }

    /// 传输相关的即时错误：写进弹窗（若还开着）并同步状态栏。
    fn transfer_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let message = message.into();
        if let Some(dialog) = self.transfer.as_mut() {
            dialog.error = Some(message.clone());
            dialog.info = None;
        }
        self.status = message;
        cx.notify();
    }

    /// Ctrl+Shift+F / 会话菜单：为当前标签打开端口转发弹窗。
    fn open_forward_action(
        &mut self,
        _: &OpenForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_forward(window, cx);
    }

    /// 打开端口转发弹窗：规则绑定当前 SSH 会话，不新开连接。
    fn open_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.forward.is_some() || self.overlay_open() {
            return;
        }
        let Some(id) = self.active_tab else {
            self.status = "没有可转发的会话，请先连接 SSH 主机".into();
            cx.notify();
            return;
        };
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        if tab.ended {
            self.status = "会话已断开，请先重新连接再配置端口转发".into();
            cx.notify();
            return;
        }
        if tab.commands.is_none() {
            self.status = "本地终端不支持端口转发，请连接 SSH 主机".into();
            cx.notify();
            return;
        }
        let title = tab.title.clone();
        let addr = tab.addr.clone();
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("备注名（可选）"));
        let listen_port = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("8080")
                .placeholder("监听端口")
        });
        let target_host = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("127.0.0.1")
                .placeholder("目标主机")
        });
        let target_port = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("80")
                .placeholder("目标端口")
        });
        listen_port.update(cx, |state, cx| state.focus(window, cx));
        self.forward = Some(ForwardDialog {
            tab_id: id,
            title,
            addr,
            editing: false,
            kind: session::ForwardKind::Local,
            name,
            listen_port,
            target_host,
            target_port,
            auto_start: false,
            error: None,
            info: None,
        });
        self.status = "端口转发：保存规则后用开关启用，可设连接时自动开启".into();
        cx.notify();
    }

    /// 切换转发方向：只改字段语义，不清空已填内容（本地/远程常用同样的 8080→80）。
    fn set_forward_kind(&mut self, kind: session::ForwardKind, cx: &mut Context<Self>) {
        let Some(dialog) = self.forward.as_mut() else {
            return;
        };
        if dialog.kind == kind {
            return;
        }
        dialog.kind = kind;
        dialog.error = None;
        dialog.info = None;
        self.status = match kind {
            session::ForwardKind::Local => {
                "本地转发：监听本机回环端口，目标按服务器视角解析".into()
            }
            session::ForwardKind::Remote => {
                "远程转发：服务器只监听其回环端口，目标按本机视角解析".into()
            }
            session::ForwardKind::Dynamic => {
                "动态转发：本机回环端口提供 SOCKS5 代理，目标由客户端请求决定".into()
            }
        };
        cx.notify();
    }

    /// 展开 / 收起新增规则表单；列表与添加分开，避免互相挤占。
    fn set_forward_editing(&mut self, editing: bool, cx: &mut Context<Self>) {
        if let Some(dialog) = self.forward.as_mut() {
            dialog.editing = editing;
            dialog.error = None;
            if !editing {
                dialog.info = None;
            }
        }
        cx.notify();
    }

    /// 表单里的「连接时自动开启」勾选。
    fn set_forward_auto_start(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(dialog) = self.forward.as_mut() {
            dialog.auto_start = on;
        }
        cx.notify();
    }

    /// 关闭端口转发弹窗；已开启的转发继续在后台工作，可在再次打开时停止。
    fn close_forward(&mut self, cx: &mut Context<Self>) {
        self.forward = None;
        cx.notify();
    }

    /// 校验表单字段，折算成规则（弹窗名称与自动开关一并返回）。
    fn forward_form_rule(
        &self,
        dialog: &ForwardDialog,
        cx: &Context<Self>,
    ) -> Result<(session::ForwardRule, String, bool), String> {
        let parse = |text: &str, label: &str| {
            text.parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| format!("{label}无效（应为 1–65535）"))
        };
        let listen_label = match dialog.kind {
            session::ForwardKind::Local => "本地端口",
            session::ForwardKind::Remote => "服务器监听端口",
            session::ForwardKind::Dynamic => "SOCKS5 端口",
        };
        let listen_port = dialog.listen_port.read(cx).value().trim().to_string();
        let listen_port = parse(&listen_port, listen_label)?;
        let name = dialog.name.read(cx).value().trim().to_string();
        if dialog.kind == session::ForwardKind::Dynamic {
            // 动态转发的目标由 SOCKS5 客户端逐条给出，表单里不填目标。
            return Ok((
                session::ForwardRule {
                    kind: dialog.kind,
                    listen_port,
                    target_host: String::new(),
                    target_port: 0,
                },
                name,
                dialog.auto_start,
            ));
        }
        let (port_label, host_hint) = match dialog.kind {
            session::ForwardKind::Local => ("远端端口", "请填写远端主机（服务器视角的地址）"),
            _ => ("本地目标端口", "请填写本地目标主机（本机视角的地址）"),
        };
        let target_host = dialog.target_host.read(cx).value().trim().to_string();
        let target_port = dialog.target_port.read(cx).value().trim().to_string();
        let target_port = parse(&target_port, port_label)?;
        if target_host.is_empty() {
            return Err(host_hint.to_string());
        }
        Ok((
            session::ForwardRule {
                kind: dialog.kind,
                listen_port,
                target_host,
                target_port,
            },
            name,
            dialog.auto_start,
        ))
    }

    /// 把表单里的规则保存到当前连接的规则列表（不动运行状态，开关另行控制）。
    fn save_forward(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.forward.as_ref() else {
            return;
        };
        let (rule, name, auto_start) = match self.forward_form_rule(dialog, cx) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.forward_error(error, cx);
                return;
            }
        };
        let addr = dialog.addr.to_string();
        let label = rule.label();
        self.forward_prefs.insert(
            &addr,
            SavedForward {
                name,
                kind: rule.kind,
                listen_port: rule.listen_port,
                target_host: rule.target_host.clone(),
                target_port: rule.target_port,
                auto_start,
            },
        );
        self.persist_forwards(cx);
        if let Some(dialog) = self.forward.as_mut() {
            // 保存后回到列表：新规则就在列表里，可以直接用开关启用。
            dialog.editing = false;
            dialog.error = None;
            dialog.info = Some(
                if auto_start {
                    "已保存，连接建立时会自动开启；也可直接用开关立即启用"
                } else {
                    "已保存，打开列表里的开关即可启用"
                }
                .into(),
            );
        }
        self.status = format!("已保存端口转发规则 {label}").into();
        cx.notify();
    }

    /// 按已保存的规则发起转发（开关打开 / 自动开启共用）。
    fn start_forward_rule(
        &mut self,
        tab_id: usize,
        rule: session::ForwardRule,
        cx: &mut Context<Self>,
    ) {
        let Some(sender) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.commands.clone())
        else {
            self.forward_error("会话已断开，无法开启端口转发", cx);
            return;
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if sender
            .send(session::SessionCommand::ForwardStart {
                rule: rule.clone(),
                reply: reply_tx,
            })
            .is_err()
        {
            self.forward_error("会话已断开，无法开启端口转发", cx);
            return;
        }
        self.status = format!("正在开启端口转发 {}…", rule.label()).into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = match reply_rx.await {
                Ok(result) => result.map_err(|error| session::error_summary(&error)),
                Err(_) => Err("会话已断开，无法开启端口转发".to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                this.finish_forward_start(tab_id, result, cx);
            });
        })
        .detach();
    }

    /// 已保存规则的开关：开＝立即启动，关＝停止（规则本身保留）。
    fn toggle_saved_forward(&mut self, saved: SavedForward, on: bool, cx: &mut Context<Self>) {
        let Some(tab_id) = self.forward.as_ref().map(|dialog| dialog.tab_id) else {
            return;
        };
        if on {
            self.start_forward_rule(tab_id, saved.rule(), cx);
        } else {
            self.stop_forward(saved.rule(), cx);
        }
    }

    /// 删除一条已保存规则；若正在转发先停掉，避免"删了还在跑"。
    fn delete_saved_forward(&mut self, saved: SavedForward, cx: &mut Context<Self>) {
        let Some(dialog) = self.forward.as_ref() else {
            return;
        };
        let tab_id = dialog.tab_id;
        let addr = dialog.addr.to_string();
        let rule = saved.rule();
        let active = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .is_some_and(|tab| tab.forwards.iter().any(|item| item.id() == rule.id()));
        if active {
            self.stop_forward(rule.clone(), cx);
        }
        self.forward_prefs.remove(&addr, &rule.id());
        self.persist_forwards(cx);
        self.status = format!("已删除端口转发规则 {}", rule.label()).into();
        cx.notify();
    }

    /// 会话就绪后自动开启勾选了「连接时自动开启」的规则。
    fn auto_start_forwards(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let Some(addr) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.addr.to_string())
        else {
            return;
        };
        let rules: Vec<session::ForwardRule> = self
            .forward_prefs
            .rules_for(&addr)
            .iter()
            .filter(|saved| saved.auto_start)
            .map(SavedForward::rule)
            .collect();
        for rule in rules {
            self.start_forward_rule(tab_id, rule, cx);
        }
    }

    /// 开启结果：成功则把生效规则记到标签上（弹窗关掉也不丢），失败写弹窗与状态栏。
    fn finish_forward_start(
        &mut self,
        tab_id: usize,
        result: Result<session::ForwardRule, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(effective) => {
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id)
                    && !tab.forwards.contains(&effective)
                {
                    tab.forwards.push(effective.clone());
                }
                let message = format!("端口转发已开启：{}", effective.label());
                let shown = self
                    .forward
                    .as_ref()
                    .is_some_and(|dialog| dialog.tab_id == tab_id);
                if shown && let Some(dialog) = self.forward.as_mut() {
                    dialog.error = None;
                    dialog.info = Some(message.clone().into());
                }
                self.status = message.into();
            }
            Err(error) => {
                if let Some(dialog) = self.forward.as_mut() {
                    dialog.info = None;
                    dialog.error = Some(error.clone().into());
                }
                self.status = format!("端口转发失败：{error}").into();
            }
        }
        cx.notify();
    }

    /// 停止一条转发，并从标签的活动规则列表里移除。
    fn stop_forward(&mut self, rule: session::ForwardRule, cx: &mut Context<Self>) {
        let Some(dialog) = self.forward.as_ref() else {
            return;
        };
        let tab_id = dialog.tab_id;
        let Some(sender) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.commands.clone())
        else {
            self.remove_forward_rule(tab_id, &rule);
            self.forward_error("会话已断开，转发已随之停止", cx);
            return;
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let label = rule.label();
        if sender
            .send(session::SessionCommand::ForwardStop {
                rule: rule.clone(),
                reply: reply_tx,
            })
            .is_err()
        {
            self.remove_forward_rule(tab_id, &rule);
            self.forward_error("会话已断开，转发已随之停止", cx);
            return;
        }
        cx.spawn(async move |this, cx| {
            let stopped = reply_rx.await.unwrap_or(false);
            let _ = this.update(cx, |this, cx| {
                this.remove_forward_rule(tab_id, &rule);
                let message = if stopped {
                    format!("已停止端口转发 {label}")
                } else {
                    format!("{label} 已不在转发中")
                };
                if let Some(dialog) = this.forward.as_mut() {
                    dialog.error = None;
                    dialog.info = Some(message.clone().into());
                }
                this.status = message.into();
                cx.notify();
            });
        })
        .detach();
    }

    /// 从标签的活动规则列表里删掉一条（服务端已停止或已失效）。
    fn remove_forward_rule(&mut self, tab_id: usize, rule: &session::ForwardRule) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            let id = rule.id();
            tab.forwards.retain(|existing| existing.id() != id);
        }
    }

    /// 端口转发的即时错误：写进弹窗（若还开着）并同步状态栏。
    fn forward_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let message = message.into();
        if let Some(dialog) = self.forward.as_mut() {
            dialog.error = Some(message.clone());
            dialog.info = None;
        }
        self.status = message;
        cx.notify();
    }

    /// 锁定凭据库：丢弃库句柄与内存中的主密钥，清空主机列表并回到解锁弹窗。
    /// 已建立的终端会话不受影响，但不再能读取保存的凭据。
    fn lock_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.vault = None;
        self.unlocked_key = None;
        self.password_cache.clear();
        self.hosts.clear();
        self.settings = None;
        self.status = "凭据库已锁定，需主密码解锁".into();
        self.open_unlock_dialog(window, cx);
        cx.notify();
    }

    /// 点击"保存主密码"或回车：设置 / 更换主密码。
    fn apply_rekey(&mut self, cx: &mut Context<Self>) {
        self.submit_master_password(MasterPasswordAction::Set, cx);
    }

    /// 点击"取消主密码"：库密钥换回系统凭据管理器里的随机密钥，此后启动无需输入口令。
    fn cancel_master_password(&mut self, cx: &mut Context<Self>) {
        self.submit_master_password(MasterPasswordAction::Cancel, cx);
    }

    /// 执行主密码设置 / 更换 / 取消：校验当前口令 → 求目标密钥（新口令派生或 keyring）
    /// → 建新库全量搬迁 → 旧库让位 → 新库就位 → 写盐与解锁标记 → 确认数据无损后清理备份。
    fn submit_master_password(&mut self, action: MasterPasswordAction, cx: &mut Context<Self>) {
        let Some(state) = self.settings.as_ref() else {
            return;
        };
        if state.busy {
            return;
        }
        let mut ctx = &mut *cx;
        let needs_current = state.current.is_some();
        let current = state
            .current
            .as_ref()
            .map(|input| input.read(&mut ctx).text().to_string());
        let password = state
            .password
            .as_ref()
            .map(|input| input.read(&mut ctx).text().to_string());
        let confirm = state
            .confirm
            .as_ref()
            .map(|input| input.read(&mut ctx).text().to_string());
        let busy = state.busy;
        if busy {
            return;
        }
        let fail = |this: &mut Self, message: SharedString, cx: &mut Context<Self>| {
            if let Some(state) = this.settings.as_mut() {
                state.error = Some(message);
            }
            cx.notify();
        };
        // 口令档免验证换钥等于没锁：应用可能在托盘里挂上几个小时。
        if needs_current {
            let verified = match (&self.unlocked_key, current.as_deref()) {
                (Some(expected), Some(entered)) => {
                    match crate::vault::load_or_create_salt(&self.mode.salt_path()) {
                        Ok(salt) => crate::vault::derive_key(&salt, entered)
                            .map(|derived| derived == *expected)
                            .unwrap_or(false),
                        Err(_) => false,
                    }
                }
                _ => false,
            };
            if !verified {
                fail(self, "当前主密码不正确".into(), cx);
                return;
            }
        }
        // 目标密钥与新盐：盐先只生成不落盘，等新库就位后才写入，
        // 否则搬迁失败会让旧口令再也派生不出旧密钥。
        let (backend, hexkey, new_salt) = match action {
            MasterPasswordAction::Set => {
                let (password, confirm) = match (&password, &confirm) {
                    (Some(password), Some(confirm)) => (password.clone(), confirm.clone()),
                    // 表单已销毁（上一次提交成功后回收）：拒绝空提交。
                    _ => {
                        fail(self, "请重新填写主密码".into(), cx);
                        return;
                    }
                };
                if password.chars().count() < 8 {
                    fail(self, "主密码至少 8 位".into(), cx);
                    return;
                }
                if password != confirm {
                    fail(self, "两次输入不一致".into(), cx);
                    return;
                }
                let salt = match crate::vault::generate_salt() {
                    Ok(salt) => salt,
                    Err(error) => {
                        fail(self, SharedString::from(error.to_string()), cx);
                        return;
                    }
                };
                let hexkey = match crate::vault::derive_key(&salt, &password) {
                    Ok(hexkey) => hexkey,
                    Err(error) => {
                        fail(self, SharedString::from(error.to_string()), cx);
                        return;
                    }
                };
                (crate::vault::UnlockBackend::Passphrase, hexkey, Some(salt))
            }
            MasterPasswordAction::Cancel => match crate::vault::load_or_create_master_key() {
                Ok(hexkey) => (crate::vault::UnlockBackend::Keyring, hexkey, None),
                Err(error) => {
                    fail(
                        self,
                        SharedString::from(format!("系统凭据管理器不可用：{error}")),
                        cx,
                    );
                    return;
                }
            },
        };
        let Some(vault) = self.vault.take() else {
            self.status = "加密库尚未就绪，无法更改主密码".into();
            cx.notify();
            return;
        };
        let mode = self.mode.clone();
        // 换名需要旧库文件句柄已释放：句柄交给本任务独占，搬迁后即 drop。
        let runtime = vault.runtime();
        if let Some(state) = self.settings.as_mut() {
            state.busy = true;
            state.error = None;
            state.info = None;
        }
        self.status = "正在重建加密库…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = async {
                let dump = vault.export_all().await?;
                let expected_hosts = dump.hosts.len();
                let salt_path = mode.salt_path();
                let previous_salt = std::fs::read(&salt_path).ok();
                // turso 无 rekey：新建加密库全量搬迁，再原子换名。
                let staging = mode.db_path().with_file_name("rulink.db.new");
                // 上一轮失败可能留下带旧数据的暂存束（含 -wal），必须先整套清掉。
                crate::vault::remove_db_bundle(&staging);
                std::fs::create_dir_all(mode.dir())?;
                let staging_db = staging.to_string_lossy().to_string();
                let fresh = Vault::open_at(&staging_db, hexkey.clone(), runtime.clone()).await?;
                fresh.import_dump(&dump).await?;
                drop(fresh);
                drop(vault);
                let db_path = mode.db_path();
                let mut backup: Option<std::path::PathBuf> = None;
                if db_path.exists() {
                    let candidate = db_path.with_file_name("rulink.db.keyring-bak");
                    let candidate = if candidate.exists() {
                        // 备份名被占用时带时间戳另存，确保旧库不丢。
                        db_path.with_file_name(format!(
                            "rulink.db.bak-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0)
                        ))
                    } else {
                        candidate
                    };
                    crate::vault::rename_db_bundle(
                        &runtime,
                        db_path.clone(),
                        candidate.clone(),
                        100,
                    )
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("旧库让位失败（数据原样保留，可直接重试）: {error}")
                    })?;
                    // 新库就位失败时把旧库放回原处，绝不留下"两头空"。
                    if let Err(error) = crate::vault::rename_db_bundle(
                        &runtime,
                        staging.clone(),
                        db_path.clone(),
                        100,
                    )
                    .await
                    {
                        let _ = crate::vault::rename_db_bundle(
                            &runtime,
                            candidate,
                            db_path.clone(),
                            100,
                        )
                        .await;
                        return Err(anyhow::anyhow!("新库就位失败（旧库已放回原处）: {error}"));
                    }
                    backup = Some(candidate);
                } else {
                    crate::vault::rename_db_bundle(&runtime, staging, db_path.clone(), 100)
                        .await
                        .map_err(anyhow::Error::msg)?;
                }
                // 新库已就位：此时才可以换盐、换标记。
                if let Some(salt) = new_salt {
                    if let Err(error) = crate::vault::write_salt(&salt_path, &salt) {
                        if let Some(previous) = previous_salt.as_deref() {
                            let _ = std::fs::write(&salt_path, previous);
                        }
                        return Err(anyhow::anyhow!("写入新派生盐失败（已还原旧盐）: {error}"));
                    }
                }
                crate::vault::write_unlock_marker(&mode, backend)?;
                let reopened = Vault::open(&mode, hexkey.clone(), runtime.clone()).await?;
                let hosts = reopened.load_hosts().await?;
                // 只有确认新库数据完整（主机数不少于导出数）才清理旧库备份，
                // 否则保留备份留待人工处置。
                if hosts.len() >= expected_hosts {
                    if let Some(path) = backup.take() {
                        crate::vault::remove_db_bundle(&path);
                        log::info!("凭据库重建完成，旧库备份已清理");
                    }
                } else {
                    log::warn!(
                        "新库主机数 {} 少于导出的 {}，保留旧库备份 {}",
                        hosts.len(),
                        expected_hosts,
                        backup
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    );
                }
                Ok::<_, anyhow::Error>((reopened, hosts, hexkey, backup))
            }
            .await;
            let done = match action {
                MasterPasswordAction::Set => "主密码已生效，下次启动需用其解锁",
                MasterPasswordAction::Cancel => {
                    "已取消主密码，改用本机凭据管理器免密解锁（库文件因此绑定本机）"
                }
            };
            let _ = this.update(cx, |this, cx| match result {
                Ok((vault, hosts, hexkey, kept_backup)) => {
                    this.vault = Some(Arc::new(vault));
                    this.hosts = hosts;
                    this.unlocked_key = Some(hexkey);
                    if let Some(state) = this.settings.as_mut() {
                        state.busy = false;
                        state.error = None;
                        // 输入框销毁：明文口令随之从界面缓冲消失，需再改时重新开启表单。
                        state.current = None;
                        state.password = None;
                        state.confirm = None;
                        state.info = Some(match kept_backup {
                            None => SharedString::from(format!("{done}；旧库备份已清理，数据完整")),
                            Some(path) => SharedString::from(format!(
                                "{done}，但新库数据不完整，旧库备份保留在 {}",
                                path.display()
                            )),
                        });
                    }
                    this.status = done.into();
                    cx.notify();
                }
                Err(error) => {
                    log::warn!("更改主密码失败: {error}");
                    if let Some(state) = this.settings.as_mut() {
                        state.busy = false;
                        state.error = Some(format!("更改失败：{error}").into());
                    }
                    this.restore_vault(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 换密钥失败后尽力恢复库句柄：keyring 档直接重开；口令档旧密钥只存在于
    /// 解锁输入中，交回设置里的解锁入口。
    fn restore_vault(&mut self, cx: &mut Context<Self>) {
        if self.vault.is_some() {
            return;
        }
        if crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase {
            self.status = "凭据库未就绪，点击侧边栏 🔑 重新解锁".into();
            return;
        }
        let mode = self.mode.clone();
        let runtime = rgpui::tokio::Tokio::handle(cx);
        cx.spawn(async move |this, cx| {
            let Ok(key) = crate::vault::load_or_create_master_key() else {
                return;
            };
            if let Ok(vault) = Vault::open(&mode, key.clone(), runtime).await {
                let _ = this.update(cx, |this, cx| {
                    this.vault = Some(Arc::new(vault));
                    this.unlocked_key = Some(key);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 一次性迁移 + JSON 导入（boot 与解锁共用）。`keyring_hexkey` 用于读取旧 vault.db。
    /// 返回库内当前主机列表，交由调用方接管界面。
    async fn after_vault_open(
        vault: &Vault,
        mode: &StorageMode,
        keyring_hexkey: Option<&str>,
        json_hosts: &[HostConfig],
    ) -> anyhow::Result<Vec<HostConfig>> {
        if let Some(key) = keyring_hexkey {
            if let Err(error) = crate::vault::migrate_legacy(vault, mode, key).await {
                log::warn!("旧数据迁移未完成: {error}");
            }
        }
        let existing = vault.load_hosts().await?;
        if existing.is_empty() && !json_hosts.is_empty() {
            let mut imported = json_hosts.to_vec();
            for (index, host) in imported.iter_mut().enumerate() {
                host.id = index as i64 + 1;
            }
            vault.import_hosts(&imported).await?;
            return Ok(imported);
        }
        Ok(existing)
    }

    /// 持久化一台主机：加密库可用则只写该行，否则整表回写旧 JSON。
    fn persist_host(&mut self, host: &HostConfig, cx: &mut Context<Self>) {
        if let Some(vault) = self.vault.clone() {
            let host = host.clone();
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.upsert(&host).await {
                    log::warn!("凭据库写入失败: {error}");
                }
            })
            .detach();
        } else {
            self.store.save(&self.hosts);
        }
    }

    /// 从 `~/.ssh/config` 导入具体 Host 条目。
    ///
    /// 通配符 / `Match` 块跳过；按 user+host+port 或同名去重；
    /// 私钥文件在本机不存在时改用 Agent（避免导入后在加载私钥这一步直接失败）。
    fn import_ssh_config(&mut self, cx: &mut Context<Self>) {
        let path = crate::ssh_config::config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                self.status = SharedString::from(format!("读取 {} 失败：{error}", path.display()));
                cx.notify();
                return;
            }
        };
        let entries = crate::ssh_config::parse(&text);
        if entries.is_empty() {
            self.status = SharedString::from(format!(
                "{} 里没有可导入的具体主机（通配符与 Match 已跳过）",
                path.display()
            ));
            cx.notify();
            return;
        }
        let fallback_user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "root".to_string());
        let mut imported = 0usize;
        let mut skipped = 0usize;
        let mut missing_keys = 0usize;
        for entry in entries {
            let user = entry.user.unwrap_or_else(|| fallback_user.clone());
            let duplicate = self.hosts.iter().any(|host| {
                (host.user.eq_ignore_ascii_case(&user)
                    && host.host.eq_ignore_ascii_case(&entry.host_name)
                    && host.port == entry.port)
                    || (!host.name.is_empty() && host.name.eq_ignore_ascii_case(&entry.alias))
            });
            if duplicate {
                skipped += 1;
                continue;
            }
            let (auth, key_path) = match entry.identity_file {
                Some(path) if path.is_file() => {
                    (AuthMethod::IdentityFile, path.display().to_string())
                }
                Some(_) => {
                    missing_keys += 1;
                    (AuthMethod::Agent, String::new())
                }
                None => (AuthMethod::Agent, String::new()),
            };
            let mut host = HostConfig {
                id: 0,
                name: entry.alias,
                host: entry.host_name,
                port: entry.port,
                user,
                auth,
                key_path,
                key_inline: false,
                favorite: false,
                proxy_jump: None,
            };
            if self.vault.is_some() {
                host.id = self.hosts.iter().map(|item| item.id).max().unwrap_or(0) + 1;
            }
            self.hosts.push(host.clone());
            self.persist_host(&host, cx);
            imported += 1;
        }
        let mut status = format!("已从 {} 导入 {imported} 台主机", path.display());
        if skipped > 0 {
            status.push_str(&format!("，跳过 {skipped} 条重复"));
        }
        if missing_keys > 0 {
            status.push_str(&format!("，{missing_keys} 条私钥不存在改用 Agent"));
        }
        self.status = SharedString::from(status);
        cx.notify();
    }

    /// 打开新建主机表单（未分组）。
    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form_at(None, None, window, cx);
    }

    /// 打开新建主机表单并预选分组（分组右键「新建主机…」进入）。
    fn open_form_in_group(
        &mut self,
        group: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_form_at(None, group, window, cx);
    }

    /// 打开新建 / 编辑表单；edit_index 为 Some 时以该主机字段预填。
    /// `group` 是新建主机的预选分组（编辑时为 None，按原归属预填）。
    fn open_form_at(
        &mut self,
        edit_index: Option<usize>,
        group: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = edit_index.and_then(|index| self.hosts.get(index).cloned());
        let editing = existing.is_some();
        let auth = existing
            .as_ref()
            .map(|host| host.auth)
            .unwrap_or(AuthMethod::Agent);
        let edit_index = if editing { edit_index } else { None };
        // 新建用调用方给的分组；编辑回填主机当前归属（可在表单里改）。
        let group = match existing.as_ref() {
            Some(host) => self
                .sidebar
                .group_of(host.id)
                .map(|name| SharedString::from(name.to_string())),
            None => group,
        };
        let placeholder_name = if editing {
            ""
        } else {
            "展示名（留空用主机地址）"
        };
        let form = NewHostForm {
            name: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder(placeholder_name);
                match &existing {
                    Some(host) => input.default_value(host.name.clone()),
                    None => input,
                }
            }),
            host: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder("host 或 IP");
                match &existing {
                    Some(host) => input.default_value(host.host.clone()),
                    None => input,
                }
            }),
            port: cx.new(|cx| {
                let input = InputState::new(window, cx).default_value("22");
                match &existing {
                    Some(host) => InputState::new(window, cx).default_value(host.port.to_string()),
                    None => input,
                }
            }),
            user: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder("登录用户名");
                match &existing {
                    Some(host) => input.default_value(host.user.clone()),
                    None => input,
                }
            }),
            key_path: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder("~/.ssh/id_ed25519");
                match &existing {
                    Some(host) => input.default_value(host.key_path.clone()),
                    None => input,
                }
            }),
            // 口令永不回填：编辑时留空即"不改动已保存的秘密"。
            passphrase: cx.new(|cx| {
                InputState::new(window, cx).placeholder("私钥口令（可选，留空则连接时按需询问）")
            }),
            key_inline: existing.as_ref().is_some_and(|host| host.key_inline),
            key_data: None,
            key_label: None,
            save_secret: false,
            auth,
            group,
            jump: existing.as_ref().and_then(|host| host.proxy_jump),
            editing: edit_index,
        };
        self.new_host = Some(form);
        // 表单打开即把焦点交给"主机地址"输入框：退格/删除等键位动作只在输入框
        // 持有焦点（"Input" 键位上下文激活）时才会派发，主动聚焦让删除键即时可用。
        if let Some(form) = self.new_host.as_ref() {
            form.host.update(cx, |state, cx| state.focus(window, cx));
        }
        self.status = if editing {
            "编辑主机".into()
        } else {
            "填写主机信息".into()
        };
        cx.notify();
    }

    /// 关闭新建表单（不保存）。
    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        self.new_host = None;
        self.status = "就绪".into();
        cx.notify();
    }

    /// 保存新建/编辑表单到列表并持久化；校验失败则保留表单。
    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.new_host.as_ref() else {
            return;
        };
        let editing = form.editing;
        let auth = form.auth;
        let passphrase = form.passphrase.read(cx).value().trim().to_string();
        let save_secret = form.save_secret;
        let group = form.group.clone();
        let jump = form.jump;
        let key_inline = form.key_inline;
        let key_data = form.key_data.clone();
        let host_raw = form.host.read(cx).value().trim().to_string();
        let key_path = form.key_path.read(cx).value().trim().to_string();
        let mut host = HostConfig {
            id: 0,
            name: form.name.read(cx).value().trim().to_string(),
            host: host_raw.clone(),
            port: form
                .port
                .read(cx)
                .value()
                .trim()
                .parse::<u16>()
                .unwrap_or(22),
            user: form.user.read(cx).value().trim().to_string(),
            auth,
            key_path: if key_inline { String::new() } else { key_path },
            key_inline: auth == AuthMethod::IdentityFile && key_inline,
            favorite: false,
            proxy_jump: jump,
        };
        if host.key_inline {
            if self.vault.is_none() {
                self.status = "内嵌私钥需要加密凭据库（当前不可用），请改用本地私钥文件".into();
                cx.notify();
                return;
            }
            let existing = editing
                .and_then(|index| self.hosts.get(index))
                .is_some_and(|current| current.key_inline);
            if key_data.is_none() && !existing {
                self.status = "请先导入私钥文件（选择「内嵌到加密库」后点「导入私钥…」）".into();
                cx.notify();
                return;
            }
        }
        if let Some(error) = host.validate() {
            log::warn!("保存主机失败: {error}");
            self.status = SharedString::from(format!("保存失败：{error}"));
            cx.notify();
            return;
        }
        // 跳板机引用必须是仍存在的另一台主机，且不能自己挂自己或形成两级以上链。
        if let Some(jump_id) = host.proxy_jump {
            let self_id = editing.and_then(|index| self.hosts.get(index)).map(|host| host.id);
            match self.hosts.iter().find(|item| item.id == jump_id) {
                None => {
                    self.status = "所选跳板机已不存在，请重新选择".into();
                    cx.notify();
                    return;
                }
                Some(jump) if Some(jump.id) == self_id => {
                    self.status = "不能把主机自己设为跳板机".into();
                    cx.notify();
                    return;
                }
                Some(jump) if jump.proxy_jump.is_some() => {
                    self.status = "跳板机不能再挂跳板机，请先解除它的跳板机设置".into();
                    cx.notify();
                    return;
                }
                Some(_) => {}
            }
        }
        let label = if host.name.is_empty() {
            host.host.clone()
        } else {
            host.name.clone()
        };
        let group_note = group
            .as_deref()
            .map(|name| format!("（分组：{name}）"))
            .unwrap_or_default();
        if let Some(index) = editing {
            // 原位替换：id 与收藏标记沿用原主机；已保存的密码/口令不受影响。
            if let Some(current) = self.hosts.get(index).cloned() {
                host.id = current.id;
                host.favorite = current.favorite;
                let was_inline = current.key_inline;
                self.hosts[index] = host.clone();
                self.persist_host(&host, cx);
                self.note_jump(&host, cx);
                let note = self.store_form_secret(&host, passphrase.clone(), save_secret, cx);
                let key_note = self.store_form_key(&host, key_data, was_inline, cx);
                self.apply_form_group(&host, group, cx);
                self.new_host = None;
                self.status = SharedString::from(format!(
                    "已更新主机 {label}{}{group_note}",
                    join_notes(note, key_note)
                ));
                cx.notify();
                return;
            }
        }
        // 新建：加密库可用时同步分配主键（max+1，与库内 INSERT 显式 id 对齐）；
        // 降级模式沿用旧 JSON（id=0，boot_vault 成功回填时会补发）。
        if self.vault.is_some() {
            host.id = self.hosts.iter().map(|item| item.id).max().unwrap_or(0) + 1;
        }
        self.hosts.push(host.clone());
        self.persist_host(&host, cx);
        self.note_jump(&host, cx);
        let note = self.store_form_secret(&host, passphrase, save_secret, cx);
        let key_note = self.store_form_key(&host, key_data, false, cx);
        self.apply_form_group(&host, group, cx);
        self.new_host = None;
        self.status = SharedString::from(format!(
            "已保存主机 {label}{}{group_note}",
            join_notes(note, key_note)
        ));
        cx.notify();
    }

    /// 把刚保存的主机落到表单选定的分组，并展开该组让新主机立刻可见。
    /// 未分组（None）同时清掉旧归属。
    fn apply_form_group(
        &mut self,
        host: &HostConfig,
        group: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let name = group.as_ref().map(|name| name.to_string());
        self.sidebar.assign_to(host.id, name.as_deref());
        if let Some(name) = name.as_deref() {
            self.sidebar.expand(name);
        }
        self.persist_sidebar(cx);
    }

    /// 表单私钥口令的两档落点：勾选入库（加密库）或仅本次内存缓存。
    /// 空白口令＝不改动既有秘密。返回附注文案（None 表示无需提示）。
    fn store_form_secret(
        &mut self,
        host: &HostConfig,
        passphrase: String,
        save: bool,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if host.auth != AuthMethod::IdentityFile || passphrase.is_empty() {
            return None;
        }
        if save {
            if let (Some(vault), true) = (self.vault.clone(), host.id != 0) {
                let id = host.id;
                self.password_cache.insert(host.addr(), passphrase.clone());
                cx.spawn(async move |_, _| {
                    if let Err(error) = vault.set_password(id, Some(&passphrase)).await {
                        log::warn!("保存私钥口令失败: {error}");
                    }
                })
                .detach();
                return Some("（口令已加密入库）".to_string());
            }
            // 加密库不可用：绝不降级为明文保存，退回本次内存持有。
            self.password_cache.insert(host.addr(), passphrase);
            return Some("（加密库不可用，口令仅本次内存持有）".to_string());
        }
        self.password_cache.insert(host.addr(), passphrase);
        Some("（口令仅本次内存持有）".to_string())
    }

    /// 内嵌私钥的落点：新导入的写入加密库，取消内嵌则清除库内旧条目。
    ///
    /// 编辑时没重新导入（key_data 为空）且原本就是内嵌的，视为"保持原样"；
    /// 删除永远不会降级为明文——加密库不可用时内嵌选项根本不可选。
    fn store_form_key(
        &mut self,
        host: &HostConfig,
        key_data: Option<String>,
        was_inline: bool,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if host.key_inline {
            let pem = key_data?;
            if let (Some(vault), true) = (self.vault.clone(), host.id != 0) {
                let id = host.id;
                self.inline_keys.insert(id, pem.clone());
                cx.spawn(async move |_, _| {
                    if let Err(error) = vault.set_host_key(id, Some(&pem)).await {
                        log::warn!("保存内嵌私钥失败: {error}");
                    }
                })
                .detach();
            }
            return Some("（私钥已加密入库）".to_string());
        }
        if was_inline {
            let id = host.id;
            self.inline_keys.remove(&id);
            if let (Some(vault), true) = (self.vault.clone(), id != 0) {
                cx.spawn(async move |_, _| {
                    if let Err(error) = vault.set_host_key(id, None).await {
                        log::warn!("清除内嵌私钥失败: {error}");
                    }
                })
                .detach();
            }
            return Some("（已移除内嵌私钥）".to_string());
        }
        None
    }

    /// 选择私钥文件并读入表单（只在保存时写入加密库，读取后原文仅存内存）。
    ///
    /// 导入即做一次格式校验：能解析或"是加密私钥"才收下，避免把随便一个文本
    /// 存进库、等到连接时才失败。
    fn import_inline_key(&mut self, cx: &mut Context<Self>) {
        if self.vault.is_none() {
            self.status = "内嵌私钥需要加密凭据库（当前不可用）".into();
            cx.notify();
            return;
        }
        if self.new_host.is_none() {
            return;
        }
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择私钥文件".into()),
        });
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.first().cloned() else {
                return;
            };
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let read = std::fs::read_to_string(&path);
            let _ = cx.with_window(entity_id, move |_, cx| {
                let _ = entity.update(cx, move |this, cx| {
                    this.apply_imported_key(label, read, cx);
                });
            });
        })
        .detach();
    }

    /// 校验并暂存导入的私钥（UI 线程侧）。
    fn apply_imported_key(
        &mut self,
        label: String,
        read: std::io::Result<String>,
        cx: &mut Context<Self>,
    ) {
        let text = match read {
            Ok(text) if text.len() > MAX_INLINE_KEY_BYTES => {
                self.status = "私钥文件过大（超过 64 KB），已拒绝导入".into();
                cx.notify();
                return;
            }
            Ok(text) => text,
            Err(error) => {
                log::warn!("读取私钥 {label} 失败: {error}");
                self.status = format!("读取私钥文件 {label} 失败").into();
                cx.notify();
                return;
            }
        };
        // 能解析、或是"加密私钥"才收下；其余内容当误选文件处理。
        if let Err(error) = russh::keys::decode_secret_key(&text, None)
            && !matches!(error, russh::keys::Error::KeyIsEncrypted)
        {
            log::warn!("导入私钥失败（{label}）: {error}");
            self.status = format!("无法识别私钥文件 {label}").into();
            cx.notify();
            return;
        }
        let Some(form) = self.new_host.as_mut() else {
            return;
        };
        form.key_data = Some(text);
        form.key_label = Some(label);
        form.key_inline = true;
        self.status = "私钥已读入，保存主机时写入加密凭据库".into();
        cx.notify();
    }

    /// 打开系统文件对话框选择私钥文件，回填表单"私钥路径"。
    fn browse_key_path(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.new_host.as_ref() else {
            return;
        };
        let input = form.key_path.clone();
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择私钥文件".into()),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.first() {
                    let value = path.to_string_lossy().into_owned();
                    let _ = cx.with_window(entity_id, move |window, cx| {
                        let _ = input.update(cx, |state, cx| {
                            state.set_value(value, window, cx);
                        });
                    });
                }
            }
        })
        .detach();
    }

    /// 右键菜单"删除"：进入二次确认，不直接动手。
    fn ask_delete(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.hosts.len() {
            self.confirm_delete = Some(index);
            cx.notify();
        }
    }

    /// 确认删除：走真实删除并持久化（连带清除库内该主机的密码/口令）。
    fn confirm_delete_now(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.confirm_delete.take() else {
            return;
        };
        self.delete_host(index, cx);
    }

    /// 取消删除确认。
    fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        cx.notify();
    }

    /// 删除指定主机并持久化。
    fn delete_host(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.hosts.len() {
            let removed = self.hosts.remove(index);
            let label = removed.name.clone();
            self.inline_keys.remove(&removed.id);
            if let Some(vault) = self.vault.clone() {
                let id = removed.id;
                cx.spawn(async move |_, _| {
                    if let Err(error) = vault.remove(id).await {
                        log::warn!("凭据库删除失败: {error}");
                    }
                })
                .detach();
            } else {
                self.store.save(&self.hosts);
            }
            self.sidebar.forget(removed.id);
            self.persist_sidebar(cx);
            // 跳板机引用一并断掉：删的是跳板机时，原本经由它的主机改回直连。
            if self.vault.is_some() {
                let orphaned = self.jumps.forget(removed.id);
                for host in &mut self.hosts {
                    if orphaned.contains(&host.id) {
                        host.proxy_jump = None;
                    }
                }
                self.persist_jumps(cx);
            }
            self.status = SharedString::from(format!("已删除主机 {label}"));
            cx.notify();
        }
    }

    /// 折叠 / 展开分组（文件夹）。
    fn toggle_group(&mut self, name: &str, cx: &mut Context<Self>) {
        self.sidebar.toggle(name);
        cx.notify();
        self.persist_sidebar(cx);
    }

    /// 打开分组新建 / 重命名弹窗；`Some(原名)` 表示重命名。
    fn open_group_prompt(
        &mut self,
        renaming: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = renaming.clone().unwrap_or_else(|| "".into());
        let placeholder = if renaming.is_some() {
            "分组名称"
        } else {
            "如：生产环境 / 家"
        };
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        if !initial.is_empty() {
            name.update(cx, |state, cx| state.set_value(initial, window, cx));
        }
        // 回车即确认，与解锁弹窗一致。
        cx.subscribe(&name, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                this.commit_group_prompt(cx);
            }
        })
        .detach();
        name.update(cx, |state, cx| state.focus(window, cx));
        self.group_prompt = Some(GroupPrompt { renaming, name });
        cx.notify();
    }

    /// 关闭分组弹窗，不写入。
    fn cancel_group_prompt(&mut self, cx: &mut Context<Self>) {
        self.group_prompt = None;
        cx.notify();
    }

    /// 确认分组弹窗：新建或重命名。空名 / 重名只提示，不关窗，便于就地改。
    fn commit_group_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.group_prompt.clone() else {
            return;
        };
        let name = prompt.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.status = "分组名称不能为空".into();
            cx.notify();
            return;
        }
        let ok = match prompt.renaming.as_deref() {
            Some(from) => self.sidebar.rename(from, &name),
            None => self.sidebar.add(&name),
        };
        if !ok {
            self.status = if prompt.renaming.is_some() {
                "重命名失败：名称为空或已存在同名分组".into()
            } else {
                "已存在同名分组".into()
            };
            cx.notify();
            return;
        }
        if let Some(from) = prompt.renaming.as_deref() {
            self.status = SharedString::from(format!("分组 {from} 已改名为 {name}"));
        } else {
            self.status = SharedString::from(format!("已新建分组 {name}"));
        }
        self.group_prompt = None;
        cx.notify();
        self.persist_sidebar(cx);
    }

    /// 删除分组：只退回归属，主机本身保留。
    fn delete_group(&mut self, name: &str, cx: &mut Context<Self>) {
        self.sidebar.remove(name);
        self.status = SharedString::from(format!("已删除分组 {name}（组内主机退回未分组）"));
        cx.notify();
        self.persist_sidebar(cx);
    }

    /// 本地 shell 的展示名：用户改过用别名，否则用扫描出的系统名。
    fn shell_name(&self, profile: &ShellProfile) -> SharedString {
        match self.local_shell_prefs.name_of(&profile.id()) {
            Some(alias) => SharedString::from(alias.to_string()),
            None => SharedString::from(profile.name.clone()),
        }
    }

    /// 搜索是否命中该 shell（自定义名、系统名或可执行文件名）。
    fn shell_matches(&self, profile: &ShellProfile, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        let alias_hit = self
            .local_shell_prefs
            .name_of(&profile.id())
            .is_some_and(|alias| alias.to_lowercase().contains(&query));
        alias_hit || profile.matches(&query)
    }

    /// 打开本地 shell 重命名弹窗（预填当前展示名）。
    fn open_shell_prompt(
        &mut self,
        profile: &ShellProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id();
        let default_text = profile.name.clone();
        let current = self.shell_name(profile).to_string();
        self.open_shell_input(
            id,
            ShellPromptKind::Rename,
            default_text.clone(),
            current,
            default_text,
            window,
            cx,
        );
    }

    /// 打开「起始目录」弹窗（预填已设置的目录，留空＝进程当前目录）。
    fn open_shell_cwd_prompt(
        &mut self,
        profile: &ShellProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id();
        let default_text = std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let current = self.local_shell_prefs.cwd_of(&id).unwrap_or("").to_string();
        self.open_shell_input(
            id,
            ShellPromptKind::WorkingDirectory,
            default_text.clone(),
            current,
            default_text,
            window,
            cx,
        );
    }

    /// 打开「启动参数」弹窗（预填已保存的参数原文，留空＝不加参数）。
    fn open_shell_args_prompt(
        &mut self,
        profile: &ShellProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id();
        let current = self.local_shell_prefs.args.get(&id).cloned().unwrap_or_default();
        self.open_shell_input(
            id,
            ShellPromptKind::Arguments,
            String::new(),
            current,
            "-NoLogo -NoProfile".into(),
            window,
            cx,
        );
    }

    /// 打开「环境变量」弹窗（预填已保存的 KEY=VALUE 行，留空＝不注入）。
    fn open_shell_env_prompt(
        &mut self,
        profile: &ShellProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id();
        let current = local_shell::env_text(&self.local_shell_prefs.env_map_of(&id));
        self.open_shell_input(
            id,
            ShellPromptKind::Environment,
            String::new(),
            current,
            "KEY=VALUE".into(),
            window,
            cx,
        );
    }

    fn open_shell_input(
        &mut self,
        id: String,
        kind: ShellPromptKind,
        default_text: String,
        current: String,
        placeholder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_line = kind.multi_line();
        let input = cx.new(|cx| {
            let state = InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(current);
            if multi_line {
                // 多行输入在无固定高度的容器里会塌成一行，用 auto_grow 撑到 6 行起。
                state.auto_grow(6, 12)
            } else {
                state
            }
        });
        // 单行弹窗回车即确认，与分组重命名一致；多行里回车是换行，点「保存」提交。
        if !multi_line {
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.commit_shell_prompt(cx);
                }
            })
            .detach();
        }
        input.update(cx, |state, cx| state.focus(window, cx));
        self.shell_prompt = Some(ShellPrompt {
            id,
            kind,
            default_text,
            input,
        });
        cx.notify();
    }

    /// 确认弹窗：重命名留空/同名即恢复默认；起始目录留空即用进程当前目录。
    fn commit_shell_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.shell_prompt.take() else {
            return;
        };
        let text = prompt.input.read(cx).value().trim().to_string();
        match prompt.kind {
            ShellPromptKind::Rename => {
                if text.is_empty() || text == prompt.default_text {
                    self.local_shell_prefs.rename(&prompt.id, "");
                    self.status =
                        SharedString::from(format!("已恢复默认名称 {}", prompt.default_text));
                } else {
                    self.local_shell_prefs.rename(&prompt.id, &text);
                    self.status = SharedString::from(format!("已重命名为 {text}"));
                }
            }
            ShellPromptKind::WorkingDirectory => {
                if text.is_empty() || text == prompt.default_text {
                    self.local_shell_prefs.set_cwd(&prompt.id, "");
                    self.status = "已恢复默认起始目录（进程当前目录）".into();
                } else if !std::path::Path::new(&text).is_dir() {
                    // 目录不存在就不写：否则每次开会话都会静默回落到默认目录，
                    // 用户以为设置生效了。
                    self.status = SharedString::from(format!("目录不存在：{text}"));
                    self.shell_prompt = Some(prompt);
                    cx.notify();
                    return;
                } else {
                    self.local_shell_prefs.set_cwd(&prompt.id, &text);
                    self.status = SharedString::from(format!("起始目录已设为 {text}"));
                }
            }
            ShellPromptKind::Arguments => {
                let args = local_shell::split_args(&text);
                if args.is_empty() {
                    self.local_shell_prefs.set_args(&prompt.id, "");
                    self.status = "已清除自定义启动参数".into();
                } else {
                    // 规范化存储：引号按分词规则重排，回填时与解析结果一致。
                    let normalized = local_shell::join_args(&args);
                    self.local_shell_prefs.set_args(&prompt.id, &normalized);
                    self.status = SharedString::from(format!("启动参数已设为 {normalized}"));
                }
            }
            ShellPromptKind::Environment => match local_shell::parse_env(&text) {
                Err(error) => {
                    self.status = SharedString::from(error);
                    self.shell_prompt = Some(prompt);
                    cx.notify();
                    return;
                }
                Ok(env) if env.is_empty() => {
                    self.local_shell_prefs.set_env(&prompt.id, "");
                    self.status = "已清除自定义环境变量".into();
                }
                Ok(env) => {
                    let normalized = local_shell::env_text(&env);
                    let count = env.len();
                    self.local_shell_prefs.set_env(&prompt.id, &normalized);
                    self.status =
                        SharedString::from(format!("已设置 {count} 个环境变量"));
                }
            },
        }
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 「浏览…」选起始目录（系统目录选择对话框），回填弹窗输入框。
    fn browse_shell_cwd(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.shell_prompt.as_ref() else {
            return;
        };
        let input = prompt.input.clone();
        let entity_id = cx.entity_id();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择起始目录".into()),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.first() {
                    let value = path.to_string_lossy().into_owned();
                    let _ = cx.with_window(entity_id, move |window, cx| {
                        let _ = input.update(cx, |state, cx| {
                            state.set_value(value, window, cx);
                        });
                    });
                }
            }
        })
        .detach();
    }

    /// 关闭重命名弹窗，不写入。
    fn cancel_shell_prompt(&mut self, cx: &mut Context<Self>) {
        self.shell_prompt = None;
        cx.notify();
    }

    /// 右键「恢复默认名称」：直接清掉别名，不弹窗。
    fn reset_shell_name(&mut self, profile: &ShellProfile, cx: &mut Context<Self>) {
        self.local_shell_prefs.rename(&profile.id(), "");
        self.status = SharedString::from(format!("已恢复默认名称 {}", profile.name));
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 右键「恢复默认起始目录」：清掉该 profile 的目录设置。
    fn reset_shell_cwd(&mut self, profile: &ShellProfile, cx: &mut Context<Self>) {
        self.local_shell_prefs.set_cwd(&profile.id(), "");
        self.status = SharedString::from(format!(
            "{} 的起始目录已恢复为进程当前目录",
            self.shell_name(profile)
        ));
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 右键「清除启动参数」。
    fn reset_shell_args(&mut self, profile: &ShellProfile, cx: &mut Context<Self>) {
        self.local_shell_prefs.set_args(&profile.id(), "");
        self.status = SharedString::from(format!("已清除 {} 的启动参数", self.shell_name(profile)));
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 右键「清除环境变量」。
    fn reset_shell_env(&mut self, profile: &ShellProfile, cx: &mut Context<Self>) {
        self.local_shell_prefs.set_env(&profile.id(), "");
        self.status = SharedString::from(format!("已清除 {} 的环境变量", self.shell_name(profile)));
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 保存本机 shell 自定义名（库可用入库，否则退回 settings.json）。
    fn persist_local_shells(&mut self, cx: &mut Context<Self>) {
        let json = self.local_shell_prefs.to_json();
        if let Some(vault) = self.vault.clone() {
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.set_setting("local_shells", &json).await {
                    log::warn!("保存本机 shell 名称失败: {error}");
                }
            })
            .detach();
        } else {
            self.store.save_local_shells(&json);
        }
    }

    /// 把主机移入分组（`None` = 未分组）。
    fn move_host(&mut self, index: usize, group: Option<SharedString>, cx: &mut Context<Self>) {
        let Some(id) = self.hosts.get(index).map(|host| host.id) else {
            return;
        };
        self.sidebar.assign_to(id, group.as_deref());
        cx.notify();
        self.persist_sidebar(cx);
    }

    /// 组内手动排序：收藏 / 非收藏各自成桶（收藏恒在桶上方，菜单只挪同桶邻居）。
    fn reorder_host(&mut self, index: usize, action: ReorderAction, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get(index) else {
            return;
        };
        let (id, favorite) = (host.id, host.favorite);
        let group = self
            .sidebar
            .group_of(id)
            .map(str::to_string)
            .unwrap_or_else(|| UNGROUPED.to_string());
        let alive: Vec<i64> = self.hosts.iter().map(|host| host.id).collect();
        self.sidebar.sync_order(&alive);
        // 同组同桶的主机按当前展示顺序（手动序 → 插入序）排好。
        let mut peers: Vec<i64> = self
            .hosts
            .iter()
            .filter(|candidate| candidate.favorite == favorite)
            .filter(|candidate| {
                self.sidebar
                    .group_of(candidate.id)
                    .map(str::to_string)
                    .unwrap_or_else(|| UNGROUPED.to_string())
                    == group
            })
            .map(|candidate| candidate.id)
            .collect();
        peers.sort_by_key(|&peer| self.sidebar.rank_of(peer));
        let Some(position) = peers.iter().position(|&peer| peer == id) else {
            return;
        };
        let anchor = match action {
            ReorderAction::Up if position > 0 => Some(peers[position - 1]),
            ReorderAction::Down if position + 1 < peers.len() => Some(peers[position + 1]),
            ReorderAction::Top if position > 0 => Some(peers[0]),
            _ => None,
        };
        let Some(anchor) = anchor else {
            let boundary = match action {
                ReorderAction::Down => "已在组内最后",
                _ => "已在组内最前",
            };
            self.status = SharedString::from(boundary);
            cx.notify();
            return;
        };
        match action {
            // 下移＝把下一个邻居挪到本机前面，等价于本机后移一位。
            ReorderAction::Down => self.sidebar.move_before(anchor, id),
            _ => self.sidebar.move_before(id, anchor),
        }
        self.persist_sidebar(cx);
        cx.notify();
    }

    /// 切换指定主机的收藏状态并持久化。
    fn toggle_favorite(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(host) = self.hosts.get_mut(index) {
            host.favorite = !host.favorite;
            let host = host.clone();
            self.persist_host(&host, cx);
            cx.notify();
        }
    }

    /// 点击主机账号：密码/私钥口令按 内存缓存 → 加密库 → 弹窗 的顺序取得；Agent 直接连接。
    fn click_host(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get(index).cloned() else {
            return;
        };
        match host.auth {
            AuthMethod::Agent => {
                let target = match self.host_target(&host, None) {
                        Ok(target) => target,
                        Err(error) => {
                            self.status = SharedString::from(error);
                            cx.notify();
                            return;
                        }
                    };
                let title = host.name.clone();
                // 带上 host：Agent 主机也能进最近连接，且重连时按最新主机配置重建。
                self.connect(title, target, None, Some(host), false, window, cx);
            }
            AuthMethod::Password | AuthMethod::IdentityFile => {
                self.click_secret_host(host, window, cx);
            }
        }
    }

    /// 密码/私钥主机的取密顺序：缓存 → 加密库 →（密码）弹窗 /（私钥）免口令直连探测。
    fn click_secret_host(&mut self, host: HostConfig, window: &mut Window, cx: &mut Context<Self>) {
        let key = host.addr();
        if let Some(secret) = self.password_cache.get(&key).cloned() {
            self.connect_with_secret(host, secret, key, false, window, cx);
            return;
        }
        if self.pending.is_some() {
            return;
        }
        // 加密库可用：异步查已存秘密，命中直连，未命中回弹窗 / 免口令试连（口令信号由会话回抛）。
        if let (Some(vault), true) = (self.vault.clone(), host.id != 0) {
            let entity = cx.entity();
            let entity_id = entity.entity_id();
            cx.spawn(async move |_, cx| {
                let saved = vault.password_of(host.id).await.ok().flatten();
                // 跳板机的凭据也从库里取：目标主机点击时才需要输入密码，
                // 跳板机不能每次都追问一遍（它只是连接路径的一部分）。
                let saved_jump = match host.proxy_jump {
                    Some(id) if id != host.id => vault.password_of(id).await.ok().flatten(),
                    _ => None,
                };
                let _ = cx.with_window(entity_id, move |window, cx| {
                    let _ = entity.update(cx, move |this, cx| {
                        if this.pending.is_some() {
                            return;
                        }
                        if let Some(secret) = saved_jump
                            && let Some(jump) = this
                                .hosts
                                .iter()
                                .find(|item| Some(item.id) == host.proxy_jump)
                        {
                            let key = jump.addr();
                            this.password_cache.insert(key, secret);
                        }
                        match (saved, host.auth) {
                            (Some(secret), _) => {
                                let key = host.addr();
                                this.connect_with_secret(host, secret, key, true, window, cx);
                            }
                            (None, AuthMethod::Password) => {
                                this.prompt_secret(host, PromptKind::Password, window, cx);
                            }
                            // 私钥：先不带口令直连；密钥确有口令时回传 PassphraseRequired 再弹窗。
                            (None, _) => this.connect_without_secret(host, window, cx),
                        }
                    });
                });
            })
            .detach();
            return;
        }
        match host.auth {
            AuthMethod::Password => self.prompt_secret(host, PromptKind::Password, window, cx),
            _ => self.connect_without_secret(host, window, cx),
        }
    }

    /// 由主机配置 + 本次秘密构造连接目标，并带上加密库里的内嵌私钥。
    ///
    /// 标记为内嵌却取不到私钥时返回 Err：宁可明确报错，也不静默退回 SSH Agent
    /// 让用户以为在用另一把钥匙。
    fn host_target(
        &self,
        host: &HostConfig,
        password: Option<String>,
    ) -> Result<SshTarget, String> {
        let key_data = if host.auth == AuthMethod::IdentityFile && host.key_inline {
            Some(self.inline_keys.get(&host.id).cloned().ok_or_else(|| {
                "内嵌私钥不可用（加密库未解锁或数据损坏），请编辑该主机重新导入".to_string()
            })?)
        } else {
            None
        };
        let jump = self.jump_target(host)?;
        Ok(SshTarget::from_host(host, password)
            .with_key_data(key_data)
            .with_jump(jump))
    }

    /// 把主机表单里选的跳板机构造成完整的连接目标（含它自己的认证材料）。
    ///
    /// 跳板机必须是另一台已保存主机；密码 / 口令只从本次运行的内存缓存里取
    /// （先单独连一次该跳板机并勾选保存即可），绝不为了跳板机再弹一次窗
    /// 或落明文。取不到时给出可操作的提示，而不是让连接失败得莫名其妙。
    fn jump_target(&self, host: &HostConfig) -> Result<Option<SshTarget>, String> {
        let Some(jump_id) = host.proxy_jump else {
            return Ok(None);
        };
        let jump = self
            .hosts
            .iter()
            .find(|item| item.id == jump_id)
            .ok_or_else(|| "所选跳板机已不存在，请编辑该主机重新选择".to_string())?;
        if jump.id == host.id {
            return Err("不能把主机自己设为跳板机".to_string());
        }
        if jump.proxy_jump.is_some() {
            return Err("跳板机不能再挂跳板机，请先解除它的跳板机设置".to_string());
        }
        let secret = self.password_cache.get(&jump.addr()).cloned();
        let password = if jump.auth == AuthMethod::Password {
            Some(secret.clone().ok_or_else(|| {
                format!(
                    "跳板机 {} 需要密码：请先单独连接它并勾选「保存密码」，或改用私钥 / Agent",
                    jump.addr()
                )
            })?)
        } else {
            None
        };
        let key_data = if jump.auth == AuthMethod::IdentityFile && jump.key_inline {
            Some(self.inline_keys.get(&jump.id).cloned().ok_or_else(|| {
                format!("跳板机 {} 的内嵌私钥不可用（加密库未解锁或数据损坏）", jump.addr())
            })?)
        } else {
            None
        };
        // 私钥口令走 `passphrase`；密码认证的 secret 已在上面作为 password 传了。
        let passphrase = if jump.auth == AuthMethod::Password {
            None
        } else {
            secret
        };
        Ok(Some(
            SshTarget::from_host(jump, password)
                .with_key_data(key_data)
                .with_passphrase(passphrase),
        ))
    }

    /// 用取到的秘密（缓存或加密库）建立连接目标并连接。
    fn connect_with_secret(
        &mut self,
        host: HostConfig,
        secret: String,
        key: String,
        from_vault: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match host.auth {
            AuthMethod::Password => self.host_target(&host, Some(secret)),
            _ => self
                .host_target(&host, None)
                .map(|target| target.with_passphrase(Some(secret))),
        };
        let target = match target {
                Ok(target) => target,
                Err(error) => {
                    self.status = SharedString::from(error);
                    cx.notify();
                    return;
                }
            };
        let title = host.name.clone();
        self.connect(title, target, Some(key), Some(host), from_vault, window, cx);
    }

    /// 不带秘密直连（私钥主机的口令探测路径）。
    fn connect_without_secret(
        &mut self,
        host: HostConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match self.host_target(&host, None) {
            Ok(target) => target,
            Err(error) => {
                self.status = SharedString::from(error);
                cx.notify();
                return;
            }
        };
        let title = host.name.clone();
        self.connect(title, target, None, Some(host), false, window, cx);
    }

    /// 弹出密码/私钥口令输入面板（默认不保存；加密库可用时提供"保存"勾选）。
    fn prompt_secret(
        &mut self,
        host: HostConfig,
        kind: PromptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = match kind {
            PromptKind::Password => "输入密码后点击连接",
            PromptKind::Passphrase => "输入私钥口令后点击连接",
        };
        let password = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        password.update(cx, |state, cx| state.focus(window, cx));
        self.pending = Some(PendingConnect {
            host,
            password,
            save_password: false,
            kind,
        });
        cx.notify();
    }

    /// 取消待输入密码的连接。
    fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        cx.notify();
    }

    /// 用输入的秘密完成待连接：内存缓存必写；勾选保存且加密库可用时再持久化入库。
    fn confirm_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let secret = pending.password.read(cx).value().to_string();
        let host = pending.host;
        let save = pending.save_password;
        let kind = pending.kind;
        let key = host.addr();
        self.password_cache.insert(key.clone(), secret.clone());
        if save && let (Some(vault), true) = (self.vault.clone(), host.id != 0) {
            let stored = secret.clone();
            let id = host.id;
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.set_password(id, Some(&stored)).await {
                    log::warn!("保存凭据失败: {error}");
                }
            })
            .detach();
        }
        let target = match kind {
            PromptKind::Password => self.host_target(&host, Some(secret)),
            PromptKind::Passphrase => self
                .host_target(&host, None)
                .map(|target| target.with_passphrase(Some(secret))),
        };
        let target = match target {
                Ok(target) => target,
                Err(error) => {
                    self.status = SharedString::from(error);
                    cx.notify();
                    return;
                }
            };
        self.connect(
            host.name.clone(),
            target,
            Some(key),
            Some(host),
            false,
            window,
            cx,
        );
    }

    /// 建立外接终端并启动 SSH 会话，追加为新标签。
    ///
    /// shell 开启前的失败（连不上 / 认证被拒 / 私钥需口令）**保留标签页**，
    /// 错误文本留在终端里可读，不再一闪而过；需要口令时自动弹口令窗。
    /// 被拒绝的秘密会清掉内存缓存；若来自加密库，库内条目也一并清除，
    /// 下次点击重新询问，而不是拿坏凭据反复直连。
    fn connect(
        &mut self,
        title: String,
        target: SshTarget,
        cache_key: Option<String>,
        host: Option<HostConfig>,
        secret_from_vault: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        // 新会话成为活动标签，分屏里没有它，回到单屏。
        self.view_split = None;
        self.active_tab = Some(id);
        self.start_ssh(
            id,
            title,
            target,
            cache_key,
            host,
            secret_from_vault,
            0,
            false,
            window,
            cx,
        );
    }

    /// 在指定标签槽位启动 SSH 会话：`reconnect` 为真时原位替换该标签的终端，
    /// 否则追加为新标签。`retry_attempt` 为自动重连的连续次数（手动连接为 0）。
    /// 会话结束（远端 exit / 断线）由终端 `CloseTerminal` 事件收尾——标签不摘除，
    /// 改为展示断开横幅。
    #[allow(clippy::too_many_arguments)]
    fn start_ssh(
        &mut self,
        id: usize,
        title: String,
        target: SshTarget,
        cache_key: Option<String>,
        host: Option<HostConfig>,
        secret_from_vault: bool,
        retry_attempt: u32,
        reconnect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(mut builder) = TerminalBuilder::new_external(None) else {
            log::error!("无法创建外接终端后端");
            return;
        };
        let Some(handles) = builder.take_external_handles() else {
            log::error!("new_external 之后缺失外接句柄");
            return;
        };
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let style = self.terminal_text_style(cx);
        let terminal_view =
            cx.new(|cx| TerminalView::new_with_style(terminal.clone(), style, window, cx));
        // 状态栏展示用的连接位置与认证方式，须在 target 被 session::start 消费前取出。
        let addr = format!("{}@{}:{}", target.user, target.host, target.port);
        let auth = if target.password.is_some() {
            AuthMethod::Password
        } else if target.key_path.is_some() || target.key_data.is_some() {
            AuthMethod::IdentityFile
        } else {
            AuthMethod::Agent
        };
        let resume = Resume {
            target: target.clone(),
            host: host.clone(),
            secret_from_vault,
        };
        let session::SshSession {
            join,
            remote_info,
            started,
            ready,
            disconnect,
            commands,
        } = session::start(
            target,
            handles,
            self.vault.clone(),
            Duration::from_secs(self.session_prefs.connect_timeout),
            cx,
        );
        // 会话结束（远端 exit / 断线 / 连接失败）→ 保留标签并展示断开横幅。
        let close_view = terminal_view.clone();
        cx.subscribe(&terminal, move |this, _, event: &TerminalEvent, cx| {
            // 只认当前标签的终端：重连后旧终端若迟来 CloseTerminal，不能把新会话标成已断开。
            if matches!(event, TerminalEvent::CloseTerminal) && this.tab_is_current(id, &close_view)
            {
                this.end_session_tab(id, cx);
            }
        })
        .detach();
        self.status = SharedString::from(format!("正在连接 {addr}…"));
        let tab = SessionTab {
            id,
            title: title.into(),
            addr: addr.clone().into(),
            auth: Some(auth),
            remote_info: None,
            terminal_view: terminal_view.clone(),
            ended: false,
            failed: false,
            resume: Some(resume),
            disconnect: Some(disconnect),
            commands: Some(commands),
            forwards: Vec::new(),
            auto_retry_attempt: retry_attempt,
            auto_retry_pending: false,
            connected_at: None,
        };
        match self.tabs.iter_mut().find(|tab| tab.id == id) {
            Some(existing) if reconnect => *existing = tab,
            Some(_) => log::warn!("标签 id {id} 已存在，忽略重复落标签"),
            None => self.tabs.push(tab),
        }
        // 新标签/重连后把焦点交回终端，键入住处即可继续敲命令。
        self.activate_tab(id, window, cx);
        // shell 真正开起来才算"连接可用"：记录时刻供断线后判断会话是否稳定过。
        let ready_view = terminal_view.clone();
        let recent_host_id = host.as_ref().map(|host| host.id).filter(|id| *id != 0);
        cx.spawn(async move |this, cx| {
            if ready.await.is_err() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                let current = this
                    .tabs
                    .iter_mut()
                    .find(|tab| tab.id == id && tab.terminal_view == ready_view);
                if let Some(tab) = current {
                    tab.connected_at = Some(Instant::now());
                    tab.auto_retry_pending = false;
                } else {
                    return;
                }
                if let Some(host_id) = recent_host_id {
                    this.record_recent_host(host_id, cx);
                }
                cx.notify();
                this.auto_start_forwards(id, cx);
            });
        })
        .detach();
        let vault = self.vault.clone();
        let current_view = terminal_view.clone();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let shell_started = started.load(std::sync::atomic::Ordering::SeqCst);
            let Some(entity) = this.upgrade() else {
                return;
            };
            let entity_id = entity.entity_id();
            let _ = cx.with_window(entity_id, move |window, cx| {
                let _ = entity.update(cx, move |this, cx| {
                    if !this.tab_is_current(id, &current_view) {
                        return;
                    }
                    // 正常结束（远端 exit / 断线）由终端的 CloseTerminal 收尾，这里无事可做。
                    let Err(error) = result else {
                        return;
                    };
                    // 加密私钥缺口令：回收死标签，自动弹口令窗。
                    if error
                        .downcast_ref::<session::PassphraseRequired>()
                        .is_some()
                    {
                        this.remove_tab(id, cx);
                        if let Some(host) = host {
                            this.prompt_secret(host, PromptKind::Passphrase, window, cx);
                        } else {
                            this.status =
                                "私钥已加密，需要口令（快速连接请改用 -i 对应明文密钥）".into();
                            cx.notify();
                        }
                        return;
                    }
                    let rejected = error.downcast_ref::<session::SecretRejected>().is_some();
                    if rejected {
                        if let Some(key) = &cache_key {
                            this.password_cache.remove(key);
                        }
                        // 凭据已被拒绝：带着同一份秘密重连只会再失败一次，
                        // 关掉重连入口，提示用户回列表重新点击以输入新凭据。
                        if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == id) {
                            tab.resume = None;
                        }
                        if secret_from_vault
                            && let (Some(vault), Some(h)) = (vault.clone(), host.as_ref())
                            && h.id != 0
                        {
                            let row = h.id;
                            cx.spawn(async move |_, _| {
                                if let Err(error) = vault.set_password(row, None).await {
                                    log::warn!("清除失效凭据失败: {error}");
                                }
                            })
                            .detach();
                            this.status = SharedString::from(
                                "已保存的凭据被服务器拒绝，已清除；再次点击将重新输入",
                            );
                        }
                    }
                    let message = if shell_started {
                        format!("连接中断：{addr}（{}）", session::error_summary(&error))
                    } else {
                        format!("连接失败：{addr}")
                    };
                    this.fail_session_tab(id, message.into(), shell_started, cx);
                });
            });
        })
        .detach();
        // 远程基本信息比会话短得多：探测回来时标签可能已关闭/重连，据此判断后再写入。
        let info_view = terminal_view.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(info) = remote_info.await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(tab) = this
                        .tabs
                        .iter_mut()
                        .find(|tab| tab.id == id && tab.terminal_view == info_view)
                    {
                        tab.remote_info = Some(info);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// 标签仍属于当前这次连接（未被同 id 的新会话替换/移除）。
    fn tab_is_current(&self, id: usize, view: &Entity<TerminalView>) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.id == id && tab.terminal_view == *view)
    }

    /// 终端报告会话结束：保留终端内容与标签，展示断开横幅（可重连）。
    fn end_session_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        if tab.ended {
            return;
        }
        tab.ended = true;
        let had_forwards = !tab.forwards.is_empty();
        // 端口转发随会话进程一起停：本地监听端口已释放，列表不能继续展示"生效中"。
        tab.forwards.clear();
        let (title, failed, active) = (tab.title.clone(), tab.failed, self.active_tab == Some(id));
        if active && !failed {
            self.status =
                SharedString::from(format!("会话已断开 {title}，按回车或点击「重新连接」恢复"));
        }
        if had_forwards
            && let Some(dialog) = self.forward.as_mut().filter(|dialog| dialog.tab_id == id)
        {
            dialog.info = None;
            dialog.error = Some("会话已断开，端口转发已全部停止".into());
        }
        cx.notify();
    }

    /// 会话以失败收场：标记断开原因，状态栏给出错误摘要（终端里另有完整错误文本）。
    ///
    /// 网络类失败按设置排定自动重连（退避 + 次数上限）；首次连接就失败不自动重试，
    /// 避免把写错的地址反复重连。
    fn fail_session_tab(
        &mut self,
        id: usize,
        message: SharedString,
        shell_started: bool,
        cx: &mut Context<Self>,
    ) {
        let mut schedule = None;
        let mut had_forwards = false;
        let auto_enabled = self.session_prefs.auto_reconnect;
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            tab.failed = true;
            // 失败也按"已结束"落标签：终端事件可能迟到，横幅与重连入口要立刻可用。
            tab.ended = true;
            had_forwards = !tab.forwards.is_empty();
            // 连接没了，本地监听端口随会话任务一起释放。
            tab.forwards.clear();
            // 稳定跑过一阵的会话：这是新一轮断线，重试次数清零。
            if tab
                .connected_at
                .is_some_and(|at| at.elapsed() >= AUTO_RECONNECT_HEALTHY)
            {
                tab.auto_retry_attempt = 0;
            }
            tab.connected_at = None;
            if self.active_tab == Some(id) {
                self.status = message;
            }
            let in_streak = tab.auto_retry_attempt > 0;
            if auto_enabled
                && tab.resume.is_some()
                && (shell_started || in_streak)
                && tab.auto_retry_attempt < AUTO_RECONNECT_MAX
            {
                let attempt = tab.auto_retry_attempt + 1;
                let delay = auto_reconnect_delay(attempt);
                tab.auto_retry_attempt = attempt;
                tab.auto_retry_pending = true;
                schedule = Some((attempt, delay, tab.terminal_view.clone()));
            }
            cx.notify();
        }
        if had_forwards
            && let Some(dialog) = self.forward.as_mut().filter(|dialog| dialog.tab_id == id)
        {
            dialog.info = None;
            dialog.error = Some("连接已断开，端口转发已全部停止".into());
        }
        if let Some((attempt, delay, view)) = schedule {
            if self.active_tab == Some(id) {
                self.status = SharedString::from(format!(
                    "连接中断，{} 秒后自动重连（第 {attempt} 次，共 {AUTO_RECONNECT_MAX} 次）",
                    delay.as_secs()
                ));
            }
            self.schedule_auto_reconnect(id, view, attempt, delay, cx);
        }
    }

    /// 延迟到点后在原标签重连；期间关标签、手动重连、取消或关掉开关都会让本次排定作废。
    fn schedule_auto_reconnect(
        &mut self,
        id: usize,
        view: Entity<TerminalView>,
        attempt: u32,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(delay).await;
            let _ = cx.with_window(entity_id, move |window, cx| {
                let _ = entity.update(cx, move |this, cx| {
                    let still_pending = this.tabs.iter().any(|tab| {
                        tab.id == id
                            && tab.terminal_view == view
                            && tab.auto_retry_pending
                            && tab.ended
                    });
                    if !still_pending || !this.session_prefs.auto_reconnect {
                        if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == id) {
                            tab.auto_retry_pending = false;
                            cx.notify();
                        }
                        return;
                    }
                    this.reconnect_tab_with(id, attempt, window, cx);
                });
            });
        })
        .detach();
    }

    /// 取消本轮自动重连：已排定的那次作废，等待用户按回车或点「重新连接」。
    fn cancel_auto_reconnect(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            tab.auto_retry_pending = false;
            tab.auto_retry_attempt = AUTO_RECONNECT_MAX;
            cx.notify();
        }
        self.status = "已取消自动重连，按回车或点击「重新连接」手动恢复".into();
    }

    /// 手动重连一个已断开的标签（重置自动重连计数）。
    fn reconnect_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.reconnect_tab_with(id, 0, window, cx);
    }

    /// 重连一个已断开的标签：优先用列表里的最新主机配置，其余沿用原连接参数。
    /// `retry_attempt` 为自动重连的连续次数（手动重连传 0）。
    fn reconnect_tab_with(
        &mut self,
        id: usize,
        retry_attempt: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((title, resume)) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id && tab.ended)
            .and_then(|tab| {
                tab.resume
                    .clone()
                    .map(|resume| (tab.title.to_string(), resume))
            })
        else {
            return;
        };
        let secret_from_vault = resume.secret_from_vault;
        let current_host = resume
            .host
            .as_ref()
            .and_then(|host| self.hosts.iter().find(|item| item.id == host.id));
        let (target, host, cache_key) = match current_host {
            Some(host) => {
                // 主机可能被编辑过：地址/端口/用户/认证方式以最新配置为准，
                // 秘密沿用原会话（仅内存），避免重连时再弹一次窗。
                let password = (host.auth == AuthMethod::Password)
                    .then(|| resume.target.password.clone())
                    .flatten();
                let Ok(mut target) = self.host_target(host, password) else {
                    self.status = "内嵌私钥不可用（加密库未解锁或数据损坏），无法重连".into();
                    cx.notify();
                    return;
                };
                if host.auth == AuthMethod::IdentityFile {
                    target.passphrase = resume.target.passphrase.clone();
                }
                (target, Some(host.clone()), Some(host.addr()))
            }
            None => (resume.target, resume.host, None),
        };
        self.start_ssh(
            id,
            title,
            target,
            cache_key,
            host,
            secret_from_vault,
            retry_attempt,
            true,
            window,
            cx,
        );
    }

    /// 用户点击标签关闭按钮。
    fn close_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        self.remove_tab(id, cx);
    }

    /// 移除标签并修正选中态；顺带把状态栏切到"已断开"。
    fn remove_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        let title = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.title.clone());
        let before = self.tabs.len();
        // 关标签＝主动断开：通知会话任务释放 SSH 连接，而不是留一条后台挂着的连接。
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id)
            && let Some(disconnect) = tab.disconnect.take()
        {
            let _ = disconnect.send(());
        }
        self.tabs.retain(|tab| tab.id != id);
        if self.tabs.len() == before {
            return;
        }
        // 分屏里的任一侧没了（关标签 / 本地 shell 退出）就回到单屏。
        if self
            .view_split
            .as_ref()
            .is_some_and(|view| view.first == id || view.second == id)
        {
            self.view_split = None;
        }
        // 传输/转发弹窗绑定的是这条连接：标签没了就一起关掉，避免对着死会话操作。
        if self
            .transfer
            .as_ref()
            .is_some_and(|dialog| dialog.tab_id == id)
        {
            self.transfer = None;
        }
        if self
            .forward
            .as_ref()
            .is_some_and(|dialog| dialog.tab_id == id)
        {
            self.forward = None;
        }
        if let Some(title) = title {
            self.status = SharedString::from(format!("已断开 {title}"));
        }
        if self.active_tab == Some(id) {
            self.active_tab = self.tabs.last().map(|tab| tab.id);
            if let Some(active) = self.active_tab {
                self.reveal_tab(active);
            }
        }
        self.sync_tray(cx);
        cx.notify();
    }

    /// 当前主题对应的调色板（从全局 Theme 语义色组装）。
    fn palette(&self, cx: &App) -> Palette {
        Palette::from_theme(cx)
    }

    /// 切换明暗主题：翻转当前模式后落到 `apply_theme`。
    fn toggle_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = cx.theme().is_dark();
        self.apply_theme(!dark, window, cx);
    }

    /// 设定明暗主题：翻转全局 Theme 模式并持久化，界面（含状态栏/按钮）随之重取配色。
    fn apply_theme(&self, dark: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if dark {
            rgpui::theme::ThemeMode::Dark
        } else {
            rgpui::theme::ThemeMode::Light
        };
        rgpui::theme::Theme::change(mode, Some(window), cx);
        if let Some(vault) = self.vault.clone() {
            let tag = if dark { "dark" } else { "light" };
            let tag = SharedString::from(tag);
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.set_setting("theme", &tag).await {
                    log::warn!("写入库内主题失败: {error}");
                }
            })
            .detach();
        } else {
            // 降级模式（无加密库）沿用旧 settings.json。
            self.store.save_theme(dark);
        }
        cx.notify();
    }

    /// Ctrl+B / 菜单项：折叠或展开侧边栏（走分隔面板状态）。
    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.split_state.update(cx, |state, cx| {
            state.toggle_collapse(CollapsiblePane::First, cx);
        });
        cx.notify();
    }

    /// 切到指定标签：选中、滚进可见范围（标签多到溢出时），并把焦点交给它的终端。
    fn activate_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        // 分屏只描述"哪两个标签并排"：从标签栏 / 快捷键切到第三个会话时退出分屏，
        // 点两个分屏面板之一则保持在分屏里（两屏位置不互换）。
        if let Some(view) = self.view_split.as_ref()
            && view.first != id
            && view.second != id
        {
            self.view_split = None;
            self.status = "已退出分屏".into();
        }
        self.active_tab = Some(id);
        if let Some(ix) = self.tabs.iter().position(|tab| tab.id == id) {
            self.tab_scroll.scroll_to_item(ix);
            let focus = self.tabs[ix].terminal_view.read(cx).focus_handle(cx);
            focus.focus(window, cx);
        }
        self.sync_tray(cx);
    }

    /// Ctrl+Shift+D / 菜单项：左右分屏；已在左右分屏时再按一次即取消。
    fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_split(SplitDirection::Horizontal, window, cx);
    }

    /// Ctrl+Shift+E / 菜单项：上下分屏；已在上下分屏时再按一次即取消。
    fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_split(SplitDirection::Vertical, window, cx);
    }

    fn close_split_action(&mut self, _: &CloseSplit, _: &mut Window, cx: &mut Context<Self>) {
        self.close_split("已取消分屏", cx);
    }

    fn close_split(&mut self, status: &str, cx: &mut Context<Self>) {
        if self.view_split.take().is_some() {
            self.status = SharedString::from(status.to_string());
            cx.notify();
        }
    }

    /// 分屏开关：同方向＝取消；换方向＝原地换排布；未分屏时把活动标签与下一个标签并排。
    fn toggle_split(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.view_split.as_mut() {
            if view.direction == direction {
                self.close_split("已取消分屏", cx);
                return;
            }
            view.direction = direction;
            let pane = view.pane.clone();
            pane.update(cx, |state, cx| state.set_direction(direction, cx));
            self.status = SharedString::from(if direction == SplitDirection::Horizontal {
                "已切换为左右分屏"
            } else {
                "已切换为上下分屏"
            });
            cx.notify();
            return;
        }
        let Some(active) = self
            .active_tab
            .filter(|id| self.tabs.iter().any(|tab| tab.id == *id))
            .or_else(|| self.tabs.last().map(|tab| tab.id))
        else {
            self.status = "没有可分屏的会话".into();
            cx.notify();
            return;
        };
        // 伙伴取活动标签的下一个（末尾则取上一个）；只有一个标签时没法分屏。
        let Some(second) = self
            .tabs
            .iter()
            .position(|tab| tab.id == active)
            .and_then(|ix| {
                let next = self.tabs.get(ix + 1).map(|tab| tab.id);
                next.or_else(|| ix.checked_sub(1).map(|prev| self.tabs[prev].id))
            })
        else {
            self.status = "分屏需要两个标签：先再开一个会话".into();
            cx.notify();
            return;
        };
        let pane = cx.new(|cx| {
            let mut state = SplitPaneState::new(cx);
            state.set_direction(direction, cx);
            // 两边都留出能看几行的宽度，拖到极小意义不大。
            state.set_min_first(240.0, cx);
            state.set_min_second(240.0, cx);
            state
        });
        // 左/上取标签顺序靠前的那个，两屏位置与标签栏顺序一致。
        let active_ix = self
            .tabs
            .iter()
            .position(|tab| tab.id == active)
            .unwrap_or(0);
        let second_ix = self
            .tabs
            .iter()
            .position(|tab| tab.id == second)
            .unwrap_or(0);
        let (first, second) = if active_ix < second_ix {
            (active, second)
        } else {
            (second, active)
        };
        self.view_split = Some(SplitView {
            first,
            second,
            direction,
            pane,
        });
        self.activate_tab(active, window, cx);
        self.status = SharedString::from(if direction == SplitDirection::Horizontal {
            "已左右分屏（拖中间分隔条调宽度）"
        } else {
            "已上下分屏（拖中间分隔条调高度）"
        });
        cx.notify();
    }

    /// 把当前会话列表刷到托盘菜单与悬停提示（隐藏到托盘后也能直接跳回某个标签）。
    fn sync_tray(&self, cx: &mut Context<Self>) {
        let mut menu = Vec::with_capacity(self.tabs.len() + 4);
        for tab in &self.tabs {
            let marker = if Some(tab.id) == self.active_tab {
                "●"
            } else {
                "○"
            };
            menu.push(TrayMenuItem::Action {
                label: format!("{marker} {}", tab.title).into(),
                id: format!("tab:{}", tab.id).into(),
            });
        }
        if !menu.is_empty() {
            menu.push(TrayMenuItem::Separator);
        }
        menu.push(TrayMenuItem::Action {
            label: "显示主界面".into(),
            id: "show".into(),
        });
        menu.push(TrayMenuItem::Separator);
        menu.push(TrayMenuItem::Action {
            label: "退出".into(),
            id: "quit".into(),
        });
        cx.set_tray_menu(menu);
        let tooltip = if self.tabs.is_empty() {
            "RuLink 拾连".to_string()
        } else {
            format!("RuLink 拾连 · {} 个会话", self.tabs.len())
        };
        cx.set_tray_tooltip(&tooltip);
    }

    /// 让标签栏滚到能看见指定标签（不改变选中态）。
    fn reveal_tab(&self, id: usize) {
        if let Some(ix) = self.tabs.iter().position(|tab| tab.id == id) {
            self.tab_scroll.scroll_to_item(ix);
        }
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab：按标签顺序循环切换（末尾回到开头）。
    fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let current = self
            .active_tab
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
            .unwrap_or(0);
        let next = (current as isize + step).rem_euclid(self.tabs.len() as isize) as usize;
        let id = self.tabs[next].id;
        self.activate_tab(id, window, cx);
        cx.notify();
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, window, cx);
    }

    fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(-1, window, cx);
    }

    /// Ctrl+T / 菜单项：切换明暗主题。
    fn toggle_theme_action(
        &mut self,
        _: &ToggleTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_theme(window, cx);
    }

    /// Ctrl+N / 菜单项：打开新建主机表单。
    fn new_host(&mut self, _: &NewHost, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form(window, cx);
    }

    /// Ctrl+, / 菜单项与 ⚙：打开设置弹窗。
    fn open_settings_action(
        &mut self,
        _: &OpenSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings(window, cx);
    }

    /// 是否有浮层占着键盘（弹窗 / 表单 / 待输密码面板）。
    fn overlay_open(&self) -> bool {
        self.unlock.is_some()
            || self.settings.is_some()
            || self.transfer.is_some()
            || self.forward.is_some()
            || self.confirm_delete.is_some()
            || self.new_host.is_some()
            || self.group_prompt.is_some()
            || self.shell_prompt.is_some()
            || self.pending.is_some()
    }

    /// 回车重连：只在焦点落在「已断开的终端」上、且没有弹窗时生效。
    ///
    /// 断开的终端仍持有焦点，但按键已无处可写；在捕获阶段拦截回车，
    /// 与 WindTerm「Remote channel is closed，按回车重新连接」的行为对齐。
    fn capture_session_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key != "enter" || self.overlay_open() {
            return;
        }
        let Some(id) = self.active_tab else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        let reconnectable = tab.ended && tab.resume.is_some();
        let terminal_focused = tab
            .terminal_view
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        if !reconnectable || !terminal_focused {
            return;
        }
        cx.stop_propagation();
        self.reconnect_tab(id, window, cx);
    }

    /// 启动时静默补扫一次：`detect()` 只查文件，WSL 得跑 `wsl.exe -l`，
    /// 不值得卡在启动路径上，但列表应当开箱即完整，所以后台补一次、不打扰状态栏。
    fn auto_scan_local_shells(&mut self, cx: &mut Context<Self>) {
        self.fetch_local_shells(true, cx);
    }

    /// 一键扫描本机 shell（含 WSL）：`wsl.exe -l` 是子进程，丢给阻塞线程池，UI 不卡。
    fn scan_local_shells(&mut self, cx: &mut Context<Self>) {
        self.fetch_local_shells(false, cx);
    }

    /// `quiet` 为真时不动状态栏（启动补扫用），扫到的列表与图标照常刷新。
    fn fetch_local_shells(&mut self, quiet: bool, cx: &mut Context<Self>) {
        if self.scanning_shells {
            return;
        }
        self.scanning_shells = true;
        if !quiet {
            self.status = "正在扫描本机 shell…".into();
        }
        cx.notify();
        let runtime = rgpui::tokio::Tokio::handle(cx);
        cx.spawn(async move |this, cx| {
            // rgpui 的 `cx.spawn` 跑在自己的执行器上，`tokio::task::spawn_blocking`
            // 会因"没有 reactor"直接 panic；阻塞任务必须显式交给 tokio 运行时句柄。
            let found = runtime.spawn_blocking(local_shell::scan).await;
            let found = found.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.scanning_shells = false;
                let count = found.len();
                this.local_shells = found;
                this.refresh_shell_icons();
                if !quiet {
                    this.status = if count == 0 {
                        "扫描完成：没找到可用的本机 shell".into()
                    } else {
                        format!(
                            "扫描完成：找到 {count} 个本机 shell，在左侧「本地终端」里点一下即可打开"
                        )
                        .into()
                    };
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 给列表里每个 shell 取一次系统图标（同一 exe 只取一次，如各 WSL 发行版共用 wsl.exe）。
    fn refresh_shell_icons(&mut self) {
        let programs: Vec<String> = self
            .local_shells
            .iter()
            .map(|profile| profile.program.clone())
            .collect();
        for program in programs {
            if self.shell_icons.contains_key(&program) {
                continue;
            }
            let Some(bytes) = local_shell::icon_png(&program) else {
                continue;
            };
            self.shell_icons.insert(
                program,
                Arc::new(Image::from_bytes(ImageFormat::Png, bytes)),
            );
        }
    }

    /// Ctrl+Shift+T / 菜单项：新开一个本地终端标签。
    fn new_local_terminal(&mut self, _: &NewLocalTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.open_local_terminal(None, cx);
    }

    /// 打开本地终端（路线图 §五 一期）：交给 rgpui-term 的本地 PTY 后端拉起选定 shell。
    /// 不指定档案时取列表第一项（`detect()` 把 PowerShell 7 排最前）。
    /// 进程拉起与通道握手是异步的：建好才落标签，失败只写状态栏，不留空标签。
    fn open_local_terminal(&mut self, profile: Option<&ShellProfile>, cx: &mut Context<Self>) {
        let chosen = profile
            .cloned()
            .or_else(|| self.local_shells.first().cloned());
        let Some(chosen) = chosen else {
            self.status = "未找到可用的本地 shell".into();
            cx.notify();
            return;
        };
        // WSL 等带参数的档案要拼成整条命令行；后端只收一个 shell 字符串。
        let profile_id = chosen.id();
        let extra_args = self.local_shell_prefs.args_of(&profile_id);
        let program = local_shell::command_line_with(&chosen, &extra_args);
        let env = self.local_shell_prefs.env_map_of(&profile_id);
        let label = self.shell_name(&chosen).to_string();
        let working_directory = self
            .local_shell_prefs
            .cwd_of(&profile_id)
            .map(std::path::PathBuf::from)
            // 目录事后被删/改名时静默回落，不阻断开会话。
            .filter(|dir| dir.is_dir())
            .or_else(|| std::env::current_dir().ok());
        let cwd = working_directory
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        self.status = SharedString::from(format!("正在启动 {label}…"));
        cx.notify();
        let Some(window_id) = cx.active_window().map(|handle| handle.window_id().as_u64()) else {
            self.status = SharedString::from(format!("启动本地终端失败：找不到当前窗口"));
            cx.notify();
            return;
        };
        let builder = TerminalBuilder::new(
            working_directory,
            Some(program),
            env,
            None,
            window_id,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let builder = builder.await;
            let Some(entity) = this.upgrade() else {
                return;
            };
            let entity_id = entity.entity_id();
            let _ = cx.with_window(entity_id, move |window, cx| {
                let _ = entity.update(cx, |this, cx| match builder {
                    Ok(builder) => this.add_local_tab(builder, label, cwd, window, cx),
                    Err(error) => {
                        log::error!("启动本地终端失败: {error}");
                        this.status = SharedString::from(format!("启动本地终端失败：{label}"));
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    /// 把建好的本地 PTY 终端落成标签；shell 进程退出即自动移除。
    fn add_local_tab(
        &mut self,
        builder: TerminalBuilder,
        label: String,
        cwd: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let style = self.terminal_text_style(cx);
        let terminal_view =
            cx.new(|cx| TerminalView::new_with_style(terminal.clone(), style, window, cx));
        let id = self.next_id;
        self.next_id += 1;
        let addr = if cwd.is_empty() {
            label.clone().into()
        } else {
            format!("{label} · {cwd}").into()
        };
        self.tabs.push(SessionTab {
            id,
            title: label.clone().into(),
            addr,
            auth: None,
            remote_info: None,
            terminal_view,
            ended: false,
            failed: false,
            resume: None,
            disconnect: None,
            commands: None,
            forwards: Vec::new(),
            auto_retry_attempt: 0,
            auto_retry_pending: false,
            connected_at: None,
        });
        self.view_split = None;
        self.active_tab = Some(id);
        self.reveal_tab(id);
        self.sync_tray(cx);
        // 本地 shell 退出（exit / Ctrl+D / 进程被杀）与 SSH 会话结束一样直接收标签。
        cx.subscribe(&terminal, move |this, _, event: &TerminalEvent, cx| {
            if matches!(event, TerminalEvent::CloseTerminal) {
                this.remove_tab(id, cx);
            }
        })
        .detach();
        self.status = SharedString::from(format!("本地终端 {label}"));
        cx.notify();
    }
}

impl Render for RuLinkApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = self.palette(cx);
        v_flex()
            .id("rulink-root")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(pal.window_bg)
            .text_color(pal.text)
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_theme_action))
            .on_action(cx.listener(Self::new_host))
            .on_action(cx.listener(Self::new_local_terminal))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::open_settings_action))
            .on_action(cx.listener(Self::open_transfer_action))
            .on_action(cx.listener(Self::open_forward_action))
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::split_down))
            .on_action(cx.listener(Self::close_split_action))
            // 捕获阶段先看到回车：断开的终端已写不到远端，回车要用来重连而不是被它吞掉。
            .capture_key_down(cx.listener(Self::capture_session_key))
            .child(self.render_title_bar(window, cx))
            .child(
                SplitPane::horizontal(self.split_state.clone())
                    .show_collapse_buttons(false)
                    .first(self.render_sidebar(cx))
                    .second(self.render_main(cx))
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden(),
            )
            .child(self.render_status_bar().flex_shrink_0().w_full())
            .child(self.render_overlay(cx))
    }
}

impl RuLinkApp {
    /// 标题栏右侧的窗口控制按钮：登记为 `WindowControlArea`，交给系统
    /// HTMINBUTTON / HTMAXBUTTON / HTCLOSE（关闭仍会走 `on_window_should_close`，
    /// 即隐藏到托盘）。
    #[allow(clippy::too_many_arguments)]
    fn window_control(
        id: &'static str,
        icon: IconName,
        area: WindowControlArea,
        fg: Hsla,
        hover_bg: Hsla,
        active_bg: Hsla,
        hover_fg: Hsla,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .w(TITLE_BAR_HEIGHT)
            .h_full()
            .flex_shrink_0()
            .justify_center()
            .content_center()
            .items_center()
            .text_color(fg)
            .hover(|style| style.bg(hover_bg).text_color(hover_fg))
            .active(|style| style.bg(active_bg).text_color(hover_fg))
            .window_control_area(area)
            .child(Icon::new(icon).small())
            .into_any_element()
    }

    /// 自定义标题栏：品牌 + 菜单（会话 / 凭据 / 外观 / 帮助）+ 快捷按钮 + 窗口控制。
    ///
    /// 这里不套框架的 `TitleBar`：它把 children 整体放进 `WindowControlArea::Drag`
    /// 命中区，而 Windows 的 WM_NCHITTEST 只取**第一个**命中的控制区，父级 Drag 先入表，
    /// 栏内按钮一律被判定为 caption，点击被系统吃掉（hover 仍在、click 永不触发）。
    /// 改成把拖拽区拆成"品牌段"与"中间空档"两个兄弟节点，交互控件落在 Drag 区之外。
    fn render_title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let bar_bg = cx.theme().tokens.title_bar;
        let bar_border = cx.theme().title_bar_border;
        let fg = cx.theme().foreground;
        let plain_hover_bg = cx.theme().secondary_hover;
        let plain_active_bg = cx.theme().secondary_active;
        let plain_hover_fg = cx.theme().secondary_foreground;
        let close_hover_bg = cx.theme().danger;
        let close_active_bg = cx.theme().danger_active;
        let close_hover_fg = cx.theme().danger_foreground;
        let dark = cx.theme().is_dark();
        let maximized = window.is_maximized();
        // 只有口令档（已设置主密码）才有"锁定/解锁"概念，keyring 档免密无锁可上。
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        let locked = passphrase_mode && self.vault.is_none();
        let vault_state: &'static str = if !passphrase_mode {
            "免主密码（系统密钥环）"
        } else if locked {
            "主密码档 · 已锁定"
        } else {
            "主密码档 · 已解锁"
        };
        let entity = cx.entity();
        // 转发跑在后台：弹窗关了也要能从标题栏看出"还有几条在生效"。
        let active_forwards = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .map_or(0, |tab| tab.forwards.len());

        // 会话菜单：新建主机、本地终端、标签切换、侧栏折叠、设置入口、托盘与退出。
        // 前六项挂 action（而非 on_click）：rgpui 会按键绑定在项右侧自动显示
        // Ctrl+N / Ctrl+Shift+T / Ctrl+Tab / Ctrl+,。
        let split_open = self.view_split.is_some();
        // 菜单打开时把最近连接的 id 解析成当前列表里的条目（主机可能已删除或重排）。
        let recent_items: Vec<(usize, SharedString)> = self
            .recent_hosts
            .iter()
            .filter_map(|id| {
                let index = self.hosts.iter().position(|host| host.id == *id)?;
                let host = &self.hosts[index];
                Some((
                    index,
                    SharedString::from(format!("{}（{}）", host.name, host.addr())),
                ))
            })
            .collect();
        let menu_entity = entity.clone();
        let session_menu = move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
            let importing = menu_entity.clone();
            let mut menu = menu
                .item(PopupMenuItem::new("新建主机…").action(Box::new(NewHost)))
                .item(
                    PopupMenuItem::new("导入 ~/.ssh/config…").on_click(move |_, _, cx| {
                        importing.update(cx, |this, cx| this.import_ssh_config(cx));
                    }),
                );
            if !recent_items.is_empty() {
                menu = menu
                    .separator()
                    .item(PopupMenuItem::new("最近连接").disabled(true));
                for (index, label) in recent_items.iter() {
                    let entity = menu_entity.clone();
                    let index = *index;
                    let label = label.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label).on_click(move |_, window, cx| {
                            entity.update(cx, |this, cx| this.click_host(index, window, cx));
                        }),
                    );
                }
            }
            let menu = menu
                .item(PopupMenuItem::new("本地终端…").action(Box::new(NewLocalTerminal)))
                .item(PopupMenuItem::new("上传 / 下载文件…").action(Box::new(OpenTransfer)))
                .item(PopupMenuItem::new("端口转发…").action(Box::new(OpenForward)))
                .item(PopupMenuItem::new("左右分屏").action(Box::new(SplitRight)))
                .item(PopupMenuItem::new("上下分屏").action(Box::new(SplitDown)));
            let menu = if split_open {
                menu.item(PopupMenuItem::new("取消分屏").action(Box::new(CloseSplit)))
            } else {
                menu
            };
            menu.item(PopupMenuItem::new("下一个标签").action(Box::new(NextTab)))
                .item(PopupMenuItem::new("上一个标签").action(Box::new(PrevTab)))
                .item(PopupMenuItem::new("折叠 / 展开侧边栏").action(Box::new(ToggleSidebar)))
                .item(PopupMenuItem::new("设置…").action(Box::new(OpenSettings)))
                .separator()
                .item(
                    PopupMenuItem::new("隐藏到托盘").on_click(move |_, window, _| {
                        window.hide_window();
                    }),
                )
                .item(
                    PopupMenuItem::new("退出 RuLink")
                        .danger(true)
                        .on_click(move |_, _, cx| cx.quit()),
                )
        };

        // 凭据菜单：解锁 / 主密码设置 / 锁定，末尾附当前档位（只读）。
        let unlock = entity.clone();
        let settings = entity.clone();
        let lock = entity.clone();
        let vault_menu = move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
            let unlock = unlock.clone();
            let settings = settings.clone();
            let lock = lock.clone();
            let menu = if locked {
                menu.item(
                    PopupMenuItem::new("解锁凭据库…").on_click(move |_, window, cx| {
                        unlock.update(cx, |this, cx| this.open_unlock_dialog(window, cx));
                    }),
                )
            } else {
                menu.item(PopupMenuItem::new("主密码与凭据库设置…").on_click(
                    move |_, window, cx| {
                        settings.update(cx, |this, cx| {
                            this.open_settings_at(SETTINGS_TAB_VAULT, window, cx);
                        });
                    },
                ))
            };
            let menu = if passphrase_mode && !locked {
                menu.item(
                    PopupMenuItem::new("锁定凭据库").on_click(move |_, window, cx| {
                        lock.update(cx, |this, cx| this.lock_vault(window, cx));
                    }),
                )
            } else {
                menu
            };
            menu.separator()
                .item(PopupMenuItem::new(vault_state).disabled(true))
        };

        // 外观菜单：明暗二选一（当前项打勾），外加设置面板的外观 / 状态栏 / 终端分页。
        let to_dark = entity.clone();
        let to_light = entity.clone();
        let appearance = entity.clone();
        let status = entity.clone();
        let terminal = entity.clone();
        let appearance_menu =
            move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
                let to_dark = to_dark.clone();
                let to_light = to_light.clone();
                let appearance = appearance.clone();
                let status = status.clone();
                let terminal = terminal.clone();
                menu.item(PopupMenuItem::new("暗色主题").checked(dark).on_click(
                    move |_, window, cx| {
                        to_dark.update(cx, |this, cx| this.apply_theme(true, window, cx));
                    },
                ))
                .item(PopupMenuItem::new("亮色主题").checked(!dark).on_click(
                    move |_, window, cx| {
                        to_light.update(cx, |this, cx| this.apply_theme(false, window, cx));
                    },
                ))
                // 明暗二选一是"设为"，这条是"翻转"，与 Ctrl+T 同义，故挂 action 显示按键。
                .item(PopupMenuItem::new("切换明暗主题").action(Box::new(ToggleTheme)))
                .separator()
                .item(
                    PopupMenuItem::new("外观设置…").on_click(move |_, window, cx| {
                        appearance.update(cx, |this, cx| {
                            this.open_settings_at(SETTINGS_TAB_APPEARANCE, window, cx);
                        });
                    }),
                )
                .item(
                    PopupMenuItem::new("状态栏显示项…").on_click(move |_, window, cx| {
                        status.update(cx, |this, cx| {
                            this.open_settings_at(SETTINGS_TAB_STATUS, window, cx);
                        });
                    }),
                )
                .item(
                    PopupMenuItem::new("终端外观…").on_click(move |_, window, cx| {
                        terminal.update(cx, |this, cx| {
                            this.open_settings_at(SETTINGS_TAB_TERMINAL, window, cx);
                        });
                    }),
                )
            };

        // 帮助菜单只留版本信息：快捷键已随各菜单项右侧显示，不再单列一份文字清单。
        let help_menu = move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
            menu.label("RuLink").item(
                PopupMenuItem::new(concat!("版本 ", env!("CARGO_PKG_VERSION"))).disabled(true),
            )
        };

        let menu_button = |id: &'static str, label: &'static str| {
            Button::new(id)
                .label(label)
                .compact()
                .ghost()
                .text_color(fg)
        };

        h_flex()
            .id("title-bar")
            .flex_shrink_0()
            .w_full()
            .h(TITLE_BAR_HEIGHT)
            .items_center()
            .pl(px(12.0))
            .border_b_1()
            .border_color(bar_border)
            .bg(bar_bg)
            .text_color(fg)
            // 可拖动段一：品牌与锁定提示。
            .child(
                h_flex()
                    .id("titlebar-brand")
                    .h_full()
                    .items_center()
                    .gap_2()
                    .pr_3()
                    .flex_shrink_0()
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight(700.0))
                            .child("RuLink"),
                    )
                    .when(locked, |this| {
                        this.child(div().text_sm().text_color(pal.muted).child("已锁定"))
                    }),
            )
            // 菜单区：交互控件，必须落在 Drag 命中区之外。
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .child(menu_button("menu-session", "会话").dropdown_menu(session_menu))
                    .child(menu_button("menu-vault", "凭据").dropdown_menu(vault_menu))
                    .child(menu_button("menu-appearance", "外观").dropdown_menu(appearance_menu))
                    .child(menu_button("menu-help", "帮助").dropdown_menu(help_menu)),
            )
            // 可拖动段二：菜单与右侧按钮之间的空档（会话标签在下方标签栏，不占标题栏）。
            .child(
                div()
                    .id("titlebar-gap")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .window_control_area(WindowControlArea::Drag),
            )
            // 右侧：传输 / 转发 + 锁定入口 + 细分隔线 + 系统窗口按钮。
            // 传输与转发绑定当前会话，放在标题栏常驻比塞进标签栏更稳定：
            // 标签一多时标签栏会横向滚动，右端按钮容易被挤出视野。
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .pr_3()
                    .child(
                        Button::new("title-transfer")
                            .label("⇅ 传输")
                            .compact()
                            .ghost()
                            .text_color(fg)
                            .tooltip("上传 / 下载文件 (Ctrl+Shift+U)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_transfer(window, cx);
                            })),
                    )
                    .child(
                        Button::new("title-forward")
                            .label(if active_forwards == 0 {
                                "⇆ 转发".to_string()
                            } else {
                                format!("⇆ 转发 {active_forwards}")
                            })
                            .compact()
                            .ghost()
                            .text_color(fg)
                            .tooltip("端口转发 (Ctrl+Shift+F)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_forward(window, cx);
                            })),
                    )
                    // 口令档且未解锁：只留重新解锁入口（其余操作都需要库）。
                    .when(locked, |this| {
                        this.child(
                            Button::new("unlock-vault")
                                // 图标集里没有钥匙/锁，保留 emoji（口令档才会出现）。
                                .label("🔑")
                                .compact()
                                .ghost()
                                .text_color(fg)
                                .tooltip("输入主密码解锁凭据库")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_unlock_dialog(window, cx);
                                })),
                        )
                    })
                    // 已设主密码才出现锁定按钮：锁定后需重新输入口令才能用库。
                    .when(passphrase_mode && !locked, |this| {
                        this.child(
                            Button::new("lock-vault")
                                .label("🔒")
                                .compact()
                                .ghost()
                                .text_color(fg)
                                .tooltip("锁定凭据库（需主密码解锁）")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.lock_vault(window, cx);
                                })),
                        )
                    }),
            )
            // 内容按钮与窗口按钮之间的细分隔线，明确"这是两条不同的按钮带"。
            .child(div().w(px(1.0)).h(px(16.0)).flex_shrink_0().bg(bar_border))
            .child(Self::window_control(
                "win-minimize",
                IconName::WindowMinimize,
                WindowControlArea::Min,
                fg,
                plain_hover_bg,
                plain_active_bg,
                plain_hover_fg,
            ))
            .child(Self::window_control(
                "win-maximize",
                if maximized {
                    IconName::WindowRestore
                } else {
                    IconName::WindowMaximize
                },
                WindowControlArea::Max,
                fg,
                plain_hover_bg,
                plain_active_bg,
                plain_hover_fg,
            ))
            .child(Self::window_control(
                "win-close",
                IconName::WindowClose,
                WindowControlArea::Close,
                fg,
                close_hover_bg,
                close_active_bg,
                close_hover_fg,
            ))
            .into_any_element()
    }

    /// 宽度与折叠由外层 SplitPane 负责，这里只填充第一面板区域。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let panel = if self.pending.is_some() {
            self.render_pending_panel(cx)
        } else {
            self.render_host_list(cx)
        };

        // 搜索框只在主机列表模式下出现（待输入密码时隐藏；表单已是居中弹窗，不占列表）。
        let list_mode = self.pending.is_none();

        v_flex()
            .size_full()
            .bg(pal.sidebar_bg)
            .p_3()
            .gap_2()
            .when(list_mode, |this| {
                this.child(styled_input(Input::new(&self.search), &pal))
            })
            .child(
                div()
                    .id("sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(panel),
            )
            .into_any_element()
    }

    /// 便携/主密码库的解锁（或首次建库）居中弹窗。
    fn render_unlock_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.unlock.as_ref() else {
            return div().into_any_element();
        };
        let is_new = state.is_new;
        v_flex()
            .id("unlock-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .items_center()
            .justify_center()
            .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(
                v_flex()
                    .id("unlock-dialog")
                    .w(px(420.0))
                    .gap_3()
                    .p_5()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .shadow_md()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_weight(rgpui::FontWeight(600.0))
                            .child(if is_new {
                                "设置主密码"
                            } else {
                                "解锁凭据库"
                            }),
                    )
                    .child(div().text_sm().text_color(pal.muted).child(if is_new {
                        "便携模式首次使用：设置主密码以创建加密库。密码仅用于派生库密钥，\
                                 不会保存到任何地方——一旦忘记无法恢复，请务必牢记。"
                    } else {
                        "输入主密码解锁凭据库（库文件与密码均不离开本机/本目录）。"
                    }))
                    .child(styled_input(
                        Input::new(&state.passphrase).content_type(InputContentType::Password),
                        &pal,
                    ))
                    .when(is_new, |this| {
                        this.child(styled_input(
                            Input::new(&state.confirm).content_type(InputContentType::Password),
                            &pal,
                        ))
                    })
                    .when_some(state.error.as_ref().cloned(), |this, error| {
                        this.child(div().text_sm().text_color(pal.danger).child(error))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("unlock-cancel")
                                    .label("退出应用")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        // 未解锁即退出：保持旧行为，不落任何秘密。
                                        {
                                            let cx = &mut *cx;
                                            this.dismiss_unlock(cx);
                                        }
                                        cx.quit();
                                    })),
                            )
                            .child(
                                Button::new("unlock-confirm")
                                    .label(if is_new { "创建并解锁" } else { "解锁" })
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.unlock_passphrase(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 设置居中弹窗：左侧竖排分页导航（凭据库与主密码 / 外观 / 状态栏 / 终端 / 会话），右侧为当前分页内容。
    fn render_settings_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.settings.as_ref() else {
            return div().into_any_element();
        };
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        let unlocked = self.vault.is_some();
        let dark = cx.theme().is_dark();
        let body = match state.tab {
            SETTINGS_TAB_APPEARANCE => self.render_appearance_tab(&pal, dark, cx),
            SETTINGS_TAB_STATUS => self.render_status_tab(&pal, cx),
            SETTINGS_TAB_TERMINAL => self.render_terminal_tab(&pal, cx),
            SETTINGS_TAB_SESSION => self.render_session_tab(&pal, cx),
            _ => self.render_vault_tab(&pal, state, passphrase_mode, unlocked, cx),
        };
        let entity = cx.entity();
        // 竖排分页条目：不套底色不描边，选中项靠左侧强调色竖条 + 粗体区分，
        // 未选中项只弱化文字色（竖条留空占位，文字位置不跳）。
        let tab_item =
            |id: &'static str, label: &'static str, ix: usize, entity: Entity<RuLinkApp>| {
                let selected = state.tab == ix;
                h_flex()
                    .id(id)
                    .h(px(30.0))
                    .items_center()
                    .gap_2()
                    .pr_3()
                    .rounded(px(6.0))
                    .text_sm()
                    .text_color(if selected { pal.text } else { pal.muted })
                    .when(selected, |item| item.font_weight(FontWeight(600.0)))
                    .hover(|style| style.bg(pal.card_hover).text_color(pal.text))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.select_settings_tab(ix, cx));
                    })
                    .child(
                        div()
                            .w(px(2.0))
                            .h(px(16.0))
                            .flex_shrink_0()
                            .rounded(px(1.0))
                            .bg(if selected { pal.accent } else { pal.card_bg }),
                    )
                    .child(label)
            };
        v_flex()
            .id("settings-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .items_center()
            .justify_center()
            .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(
                v_flex()
                    .id("settings-dialog")
                    // 尺寸固定：切分页不再让弹窗忽大忽小，超出部分交给内容区滚动。
                    .w(px(SETTINGS_DIALOG_W))
                    .h(px(SETTINGS_DIALOG_H))
                    .flex_shrink_0()
                    .gap_3()
                    .p_5()
                    .overflow_hidden()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .shadow_md()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight(600.0))
                            .flex_shrink_0()
                            .child("设置"),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .items_stretch()
                            .gap_4()
                            .child(
                                v_flex()
                                    .w(px(SETTINGS_NAV_W))
                                    .flex_shrink_0()
                                    .gap_1()
                                    .child(tab_item(
                                        "settings-tab-vault",
                                        "凭据库与主密码",
                                        SETTINGS_TAB_VAULT,
                                        entity.clone(),
                                    ))
                                    .child(tab_item(
                                        "settings-tab-appearance",
                                        "外观",
                                        SETTINGS_TAB_APPEARANCE,
                                        entity.clone(),
                                    ))
                                    .child(tab_item(
                                        "settings-tab-status",
                                        "状态栏",
                                        SETTINGS_TAB_STATUS,
                                        entity.clone(),
                                    ))
                                    .child(tab_item(
                                        "settings-tab-terminal",
                                        "终端",
                                        SETTINGS_TAB_TERMINAL,
                                        entity.clone(),
                                    ))
                                    .child(tab_item(
                                        "settings-tab-session",
                                        "会话",
                                        SETTINGS_TAB_SESSION,
                                        entity,
                                    )),
                            )
                            // 导航与内容之间只留一条分隔线，不再各套一个框。
                            .child(div().w(px(1.0)).flex_shrink_0().bg(pal.border))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    // 显式定高：滚动容器要有确定边界，否则内容只会溢出被裁。
                                    .h(px(SETTINGS_CONTENT_H))
                                    .pl_5()
                                    .overflow_y_scrollbar()
                                    .child(body),
                            ),
                    )
                    .child(
                        h_flex().flex_shrink_0().gap_2().justify_end().child(
                            Button::new("settings-close")
                                .label("关闭")
                                .secondary()
                                .outline()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_settings(cx);
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// 文件传输弹窗：本地文件 + 远端路径，上传 / 下载共用，传输中显示进度。
    fn render_transfer_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(dialog) = self.transfer.as_ref() else {
            return div().into_any_element();
        };
        let busy = dialog.busy;
        let direction = dialog.direction;
        let (done, total) = dialog.progress.unwrap_or((0, 0));
        let fraction = if total > 0 {
            (done as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let progress_text = match direction {
            Some(direction) if busy => format!(
                "{}中 · {} / {}（{:.0}%）",
                direction.label(),
                transfer::format_bytes(done),
                transfer::format_bytes(total),
                fraction * 100.0
            ),
            Some(direction) => format!(
                "上次{} · {}",
                direction.label(),
                transfer::format_bytes(done)
            ),
            None => String::new(),
        };
        v_flex()
            .id("transfer-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .items_center()
            .justify_center()
            .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if !this.transfer.as_ref().is_some_and(|dialog| dialog.busy) {
                        this.close_transfer(cx);
                    }
                }),
            )
            .child(
                v_flex()
                    .id("transfer-dialog")
                    .w(px(540.0))
                    .gap_3()
                    .p_5()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .shadow_md()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight(600.0))
                                    .child(format!("文件传输 · {}", dialog.title)),
                            )
                            .child(
                                Button::new("close-transfer").label("×").ghost().on_click(
                                    cx.listener(|this, _, _, cx| this.close_transfer(cx)),
                                ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child("本地路径"))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(styled_input(Input::new(&dialog.local), &pal).flex_1())
                                    .child(
                                        Button::new("transfer-browse")
                                            .label("浏览…")
                                            .secondary()
                                            .outline()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.browse_transfer_local(cx);
                                            })),
                                    ),
                            )
                            .child(
                                div().text_xs().text_color(pal.muted).child(
                                    "选择本地文件用于上传；下载时点「下载到…」另选保存位置。",
                                ),
                            ),
                    )
                    .child(labeled_field("远端路径", &dialog.remote, &pal))
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child("支持 ~ 展开；上传到目录（如主目录）时自动沿用本地文件名。"),
                    )
                    .when(!progress_text.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(if busy { pal.text } else { pal.muted })
                                        .child(progress_text),
                                )
                                .child(
                                    div()
                                        .h(px(6.0))
                                        .w_full()
                                        .rounded(px(3.0))
                                        .bg(pal.border)
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .h_full()
                                                .w(rgpui::relative(fraction))
                                                .rounded(px(3.0))
                                                .bg(pal.accent),
                                        ),
                                ),
                        )
                    })
                    .when_some(dialog.error.clone(), |this, error| {
                        this.child(div().text_sm().text_color(pal.danger).child(error))
                    })
                    .when_some(dialog.info.clone(), |this, info| {
                        this.child(div().text_xs().text_color(pal.muted).child(info))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("transfer-close")
                                    .label("关闭")
                                    .ghost()
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.close_transfer(cx)),
                                    ),
                            )
                            .child(
                                Button::new("transfer-download")
                                    .label("下载到…")
                                    .secondary()
                                    .outline()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.download_transfer(cx);
                                    })),
                            )
                            .child(
                                Button::new("transfer-upload")
                                    .label("上传")
                                    .primary()
                                    .loading(busy && direction == Some(transfer::Direction::Upload))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.upload_transfer(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 端口转发弹窗：已保存规则的开关列表 + 新建规则表单。
    fn render_forward_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(dialog) = self.forward.as_ref() else {
            return div().into_any_element();
        };
        let active = self
            .tabs
            .iter()
            .find(|tab| tab.id == dialog.tab_id)
            .map(|tab| tab.forwards.clone())
            .unwrap_or_default();
        let saved_rules = self.forward_prefs.rules_for(&dialog.addr).to_vec();
        let auto_start = dialog.auto_start;
        let editing = dialog.editing;
        let entity = cx.entity();
        let kind = dialog.kind;
        let (listen_label, host_label, port_label, hint) = match kind {
            session::ForwardKind::Local => (
                "本地端口",
                "远端主机",
                "远端端口",
                "监听只绑本机 127.0.0.1，不对局域网开放；目标地址按服务器视角解析。",
            ),
            session::ForwardKind::Remote => (
                "服务器监听端口",
                "本地目标主机",
                "本地目标端口",
                "服务器只监听其 127.0.0.1，不对公网开放；目标地址按本机视角解析。",
            ),
            session::ForwardKind::Dynamic => (
                "SOCKS5 端口",
                "",
                "",
                "在本机 127.0.0.1 上提供 SOCKS5 代理（无认证），目标地址由客户端请求给出、经服务器解析。",
            ),
        };
        let is_dynamic = kind == session::ForwardKind::Dynamic;
        let local_button = if kind == session::ForwardKind::Local {
            Button::new("forward-kind-local")
                .label("本地 -L")
                .primary()
        } else {
            Button::new("forward-kind-local")
                .label("本地 -L")
                .ghost()
        }
        .on_click(cx.listener(|this, _, _, cx| {
            this.set_forward_kind(session::ForwardKind::Local, cx);
        }));
        let remote_button = if kind == session::ForwardKind::Remote {
            Button::new("forward-kind-remote")
                .label("远程 -R")
                .primary()
        } else {
            Button::new("forward-kind-remote")
                .label("远程 -R")
                .ghost()
        }
        .on_click(cx.listener(|this, _, _, cx| {
            this.set_forward_kind(session::ForwardKind::Remote, cx);
        }));
        let dynamic_button = if is_dynamic {
            Button::new("forward-kind-dynamic")
                .label("动态 -D")
                .primary()
        } else {
            Button::new("forward-kind-dynamic")
                .label("动态 -D")
                .ghost()
        }
        .on_click(cx.listener(|this, _, _, cx| {
            this.set_forward_kind(session::ForwardKind::Dynamic, cx);
        }));
        v_flex()
            .id("forward-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .items_center()
            .justify_center()
            .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_forward(cx)),
            )
            .child(
                v_flex()
                    .id("forward-dialog")
                    .w(px(680.0))
                    .max_h(px(560.0))
                    .gap_3()
                    .p_5()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .shadow_md()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight(600.0))
                                    .child(format!("端口转发 · {}", dialog.title)),
                            )
                            .child(
                                Button::new("close-forward")
                                    .label("×")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| this.close_forward(cx))),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(pal.muted)
                                    .child("已保存的转发（开关即时生效）"),
                            )
                            .child(
                                v_flex()
                                    .id("forward-saved-list")
                                    .gap_1()
                                    .max_h(if editing { px(150.0) } else { px(260.0) })
                                    .overflow_y_scrollbar()
                                    .when(saved_rules.is_empty(), |this| {
                                        this.child(
                                            div().text_sm().text_color(pal.muted).child(
                                                "暂无保存的规则，点下方「设置…」添加一条。",
                                            ),
                                        )
                                    })
                                    .children(saved_rules.iter().cloned().map(|saved| {
                                        let rule = saved.rule();
                                        let on = active.iter().any(|item| item.id() == rule.id());
                                        let switch_saved = saved.clone();
                                        let delete_saved = saved.clone();
                                        let row_entity = entity.clone();
                                        h_flex()
                                            .id(SharedString::from(format!(
                                                "forward-row-{}",
                                                rule.id()
                                            )))
                                            .justify_between()
                                            .items_center()
                                            .gap_3()
                                            .py_1()
                                            .child(
                                                h_flex()
                                                    .flex_1()
                                                    .items_center()
                                                    .gap_3()
                                                    .child(forward_switch(
                                                        format!("forward-toggle-{}", rule.id()),
                                                        on,
                                                        &pal,
                                                        move |_, _, cx| {
                                                            let on = !on;
                                                            row_entity.update(cx, |this, cx| {
                                                                this.toggle_saved_forward(
                                                                    switch_saved.clone(),
                                                                    on,
                                                                    cx,
                                                                );
                                                            });
                                                        },
                                                    ))
                                                    .child(
                                                        v_flex()
                                                            .gap_0p5()
                                                            .child(
                                                                div()
                                                                    .text_sm()
                                                                    .font_weight(FontWeight(600.0))
                                                                    .child(saved.title()),
                                                            )
                                                            // 没备注时标题本身就是规则描述，不再重复一行。
                                                            .when(
                                                                !saved.name.trim().is_empty(),
                                                                |this| {
                                                                    this.child(
                                                                        div()
                                                                            .text_xs()
                                                                            .text_color(pal.muted)
                                                                            .child(rule.label()),
                                                                    )
                                                                },
                                                            ),
                                                    ),
                                            )
                                            .child(
                                                Button::new(SharedString::from(format!(
                                                    "forward-delete-{}",
                                                    rule.id()
                                                )))
                                                .label("删除")
                                                .compact()
                                                .ghost()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.delete_saved_forward(
                                                        delete_saved.clone(),
                                                        cx,
                                                    );
                                                })),
                                            )
                                    })),
                            ),
                    )
                    .when(editing, |this| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .items_end()
                                .child(local_button)
                                .child(remote_button)
                                .child(dynamic_button)
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .gap_1()
                                        .child(div().text_sm().child("名称"))
                                        .child(styled_input(Input::new(&dialog.name), &pal)),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    v_flex()
                                        .w(px(130.0))
                                        .gap_1()
                                        .child(div().text_sm().child(listen_label))
                                        .child(styled_input(
                                            Input::new(&dialog.listen_port),
                                            &pal,
                                        )),
                                )
                                .when(!is_dynamic, |this| {
                                    this.child(
                                        v_flex()
                                            .flex_1()
                                            .gap_1()
                                            .child(div().text_sm().child(host_label))
                                            .child(styled_input(
                                                Input::new(&dialog.target_host),
                                                &pal,
                                            )),
                                    )
                                    .child(
                                        v_flex()
                                            .w(px(130.0))
                                            .gap_1()
                                            .child(div().text_sm().child(port_label))
                                            .child(styled_input(
                                                Input::new(&dialog.target_port),
                                                &pal,
                                            )),
                                    )
                                }),
                        )
                        .child(div().text_xs().text_color(pal.muted).child(hint))
                        .when_some(dialog.error.clone(), |this, error| {
                            this.child(div().text_sm().text_color(pal.danger).child(error))
                        })
                        .child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(forward_switch(
                                            "forward-auto-start".to_string(),
                                            auto_start,
                                            &pal,
                                            {
                                                let entity = entity.clone();
                                                move |_, _, cx| {
                                                    let on = !auto_start;
                                                    entity.update(cx, |this, cx| {
                                                        this.set_forward_auto_start(on, cx);
                                                    });
                                                }
                                            },
                                        ))
                                        .child(div().text_sm().child("连接时自动开启")),
                                )
                                .child(div().flex_1())
                                .child(
                                    Button::new("forward-cancel")
                                        .label("取消")
                                        .ghost()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_forward_editing(false, cx);
                                        })),
                                )
                                .child(
                                    Button::new("forward-save")
                                        .label("保存规则")
                                        .primary()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.save_forward(cx);
                                        })),
                                ),
                        )
                    })
                    .when(!editing, |this| {
                        this.when_some(dialog.error.clone(), |this, error| {
                            this.child(div().text_sm().text_color(pal.danger).child(error))
                        })
                        .when_some(dialog.info.clone(), |this, info| {
                            this.child(div().text_xs().text_color(pal.muted).child(info))
                        })
                        .child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .child(
                                    Button::new("forward-settings")
                                        .label("设置…")
                                        .secondary()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_forward_editing(true, cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(pal.muted)
                                        .child("添加或修改转发规则"),
                                )
                                .child(div().flex_1())
                                .child(
                                    Button::new("forward-close")
                                        .label("关闭")
                                        .ghost()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.close_forward(cx)
                                        })),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    /// 「凭据库与主密码」分页：库位置与解锁方式、主密码设置/更换/取消、一键锁定。
    fn render_vault_tab(
        &self,
        pal: &Palette,
        state: &SettingsState,
        passphrase_mode: bool,
        unlocked: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        v_flex()
            .id("settings-tab-vault")
            .gap_3()
            .child(div().text_sm().text_color(pal.muted).child(format!(
                "位置：{}　解锁方式：{}",
                self.mode.dir().display(),
                if passphrase_mode {
                    "主密码（Argon2id 派生）"
                } else {
                    "系统凭据管理器（无需输入）"
                }
            )))
            .child(
                div()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(if passphrase_mode {
                        "主密码只用于派生库密钥，不会写入任何文件；忘记主密码将无法恢复数据。\
                         每次启动需输入主密码，库文件可凭口令带往其他机器解锁。"
                    } else {
                        "当前由系统凭据管理器免密解锁，开箱即用。建议设置主密码：库文件可凭口令\
                         带往其他机器，他人取到文件也无法读取；忘记主密码将无法恢复数据。"
                    }),
            )
            .when(!unlocked, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(pal.danger)
                        .child("凭据库未解锁，无法设置或更换主密码。"),
                )
            })
            .when(unlocked, |this| {
                let form = state.password.as_ref().zip(state.confirm.as_ref());
                match form {
                    Some((password, confirm)) => {
                        let this =
                            this.child(div().text_base().font_weight(FontWeight(600.0)).child(
                                if passphrase_mode {
                                    "更换主密码"
                                } else {
                                    "设置主密码（可选）"
                                },
                            ));
                        let this = match state.current.as_ref() {
                            Some(current) => this.child(styled_input(
                                Input::new(current).content_type(InputContentType::Password),
                                &pal,
                            )),
                            None => this,
                        };
                        this.child(styled_input(
                            Input::new(password).content_type(InputContentType::Password),
                            &pal,
                        ))
                        .child(styled_input(
                            Input::new(confirm).content_type(InputContentType::Password),
                            &pal,
                        ))
                        .when_some(state.error.as_ref().cloned(), |this, error| {
                            this.child(div().text_sm().text_color(pal.danger).child(error))
                        })
                        .when(state.busy, |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(pal.muted)
                                    .child("正在新建加密库并全量搬迁，请稍候…"),
                            )
                        })
                        .when(!state.busy, |this| {
                            this.child(
                                h_flex()
                                    .gap_2()
                                    .justify_end()
                                    .when(passphrase_mode, |this| {
                                        this.child(
                                            Button::new("rekey-cancel")
                                                .label("取消主密码")
                                                .secondary()
                                                .outline()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cancel_master_password(cx);
                                                })),
                                        )
                                    })
                                    .child(
                                        Button::new("rekey-apply")
                                            .label(if passphrase_mode {
                                                "更换主密码"
                                            } else {
                                                "设置主密码"
                                            })
                                            .primary()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.apply_rekey(cx);
                                            })),
                                    ),
                            )
                        })
                    }
                    // 提交成功后表单已销毁（明文口令不留界面），只留结果与重开入口。
                    None => this
                        .when_some(state.info.as_ref().cloned(), |this, info| {
                            this.child(div().text_sm().text_color(pal.accent).child(info))
                        })
                        .when_some(state.error.as_ref().cloned(), |this, error| {
                            this.child(div().text_sm().text_color(pal.danger).child(error))
                        })
                        .when(!state.busy, |this| {
                            this.child(
                                h_flex().gap_2().justify_end().child(
                                    Button::new("rekey-reopen")
                                        .label(if passphrase_mode {
                                            "更换主密码"
                                        } else {
                                            "设置主密码"
                                        })
                                        .ghost()
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reopen_password_form(window, cx);
                                        })),
                                ),
                            )
                        }),
                }
            })
            .when(passphrase_mode && unlocked, |this| {
                this.child(
                    h_flex().gap_2().justify_end().child(
                        Button::new("settings-lock")
                            .label("锁定凭据库")
                            .secondary()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.lock_vault(window, cx);
                            })),
                    ),
                )
            })
            .into_any_element()
    }

    /// 「外观」分页：明暗主题（与 `Ctrl+T`、标题栏 ☀☾ 同一开关）。
    fn render_appearance_tab(&self, pal: &Palette, dark: bool, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .id("settings-tab-appearance")
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(pal.muted)
                    .child("主题切换立即生效并写入凭据库，下次启动沿用。"),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(pal.muted)
                            .child(format!("当前主题：{}", if dark { "暗色" } else { "亮色" })),
                    )
                    .child(
                        Button::new("settings-toggle-theme")
                            .label(if dark {
                                "切换到亮色"
                            } else {
                                "切换到暗色"
                            })
                            .secondary()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_theme(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// 「状态栏」分页：勾选底部状态栏要显示哪些条目，改动即时生效并写入凭据库。
    fn render_status_tab(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let row =
            |id: String, label: String, on: bool, pref: StatusPref, entity: Entity<RuLinkApp>| {
                Checkbox::new(id)
                    .label(label)
                    .checked(on)
                    .on_change(move |value, _window, cx| {
                        entity.update(cx, |this, cx| this.set_status_pref(pref, value, cx));
                    })
            };
        let mut metrics = Vec::new();
        for metric in Metric::ALL {
            metrics.push(row(
                format!("status-{}", metric.id()),
                metric.label().to_string(),
                self.status_bar.metric_on(metric),
                StatusPref::Metric(metric),
                entity.clone(),
            ));
        }
        v_flex()
            .id("settings-tab-status")
            .gap_3()
            .child(
                div().text_sm().text_color(pal.muted).child(
                    "勾选底部状态栏要显示的条目，取消勾选即隐藏；改动立即生效并写入凭据库。",
                ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_sm().text_color(pal.muted).child("会话信息"))
                    .child(row(
                        "status-addr".into(),
                        "连接地址（user@host:port）".into(),
                        self.status_bar.addr,
                        StatusPref::Addr,
                        entity.clone(),
                    ))
                    .child(row(
                        "status-auth".into(),
                        "认证方式".into(),
                        self.status_bar.auth,
                        StatusPref::Auth,
                        entity,
                    )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div().text_sm().text_color(pal.muted).child(
                            "远程主机信息（连接后经 exec 探测取得，远端没探到的项自动不显示）",
                        ),
                    )
                    .children(metrics),
            )
            .into_any_element()
    }

    /// 「终端」分页：字号、行高与配色方案，改动即刻刷到所有已开终端并写入凭据库。
    fn render_terminal_tab(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let themes = Self::terminal_themes(cx);
        let theme_index = themes
            .iter()
            .position(|name| *name == self.terminal.theme)
            .unwrap_or(0);
        // 步进按钮：− / + 各一档，值显示在中间，比数字框少一层提交路径。
        let row = |label: &'static str, value: String, minus, plus| {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .w(px(72.0))
                        .text_sm()
                        .text_color(pal.muted)
                        .child(label),
                )
                .child(minus)
                .child(
                    div()
                        .w(px(64.0))
                        .text_center()
                        .text_sm()
                        .flex_shrink_0()
                        .child(value),
                )
                .child(plus)
        };
        v_flex()
            .id("settings-tab-terminal")
            .gap_3()
            .child(div().text_sm().text_color(pal.muted).child(
                "终端外观只影响会话终端（界面主题在「外观」分页里），改动立即生效并写入凭据库。",
            ))
            .child(row(
                "字号",
                format!("{} px", self.terminal.font_size as i32),
                stepper(
                    "terminal-font-minus",
                    "−",
                    cx.listener(|this, _, _, cx| this.nudge_font_size(-1, cx)),
                ),
                stepper(
                    "terminal-font-plus",
                    "+",
                    cx.listener(|this, _, _, cx| this.nudge_font_size(1, cx)),
                ),
            ))
            .child(row(
                "行高",
                format!("{:.1}", self.terminal.line_height),
                stepper(
                    "terminal-line-minus",
                    "−",
                    cx.listener(|this, _, _, cx| this.nudge_line_height(-1, cx)),
                ),
                stepper(
                    "terminal-line-plus",
                    "+",
                    cx.listener(|this, _, _, cx| this.nudge_line_height(1, cx)),
                ),
            ))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(72.0))
                            .text_sm()
                            .text_color(pal.muted)
                            .child("配色方案"),
                    )
                    .child({
                        let options = themes.clone();
                        Select::new(themes)
                            .id("terminal-theme")
                            .selected(Some(theme_index))
                            .w(px(200.0))
                            .on_change(move |index, _name, _window, cx| {
                                let Some(name) = options.get(index).cloned() else {
                                    return;
                                };
                                let entity = entity.clone();
                                entity.update(cx, |this, cx| {
                                    this.set_terminal_theme(&name, cx);
                                });
                            })
                    }),
            )
            .when(self.terminal.theme == TERMINAL_THEME_CUSTOM, |this| {
                this.child(self.render_custom_colors(pal, cx))
            })
            .child(self.render_local_shell_row(pal, cx))
            .child(
                div().text_xs().text_color(pal.muted).child(
                    "「一键扫描」会实际调用 wsl.exe 列出 WSL 发行版，故放在按钮后面而不是启动时；"
                        .to_string()
                        + "扫描结果列在左侧「本地终端」分组里，点一下即开对应终端。",
                ),
            )
            .into_any_element()
    }

    /// 自定义 16 色编辑器：前景 / 背景两行，下面 8 行「普通 | 明亮」同色成对。
    /// 输入框只认 `#RGB` / `#RRGGBB`，非法内容不提交，色块保持上一个有效颜色。
    fn render_custom_colors(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let colors = &self.terminal.custom;
        // 单个色块 + 输入框；下标按 `TerminalColors` 字段序列取。
        let cell = |index: usize| {
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    div()
                        .size(px(14.0))
                        .flex_shrink_0()
                        .rounded(px(3.0))
                        .border_1()
                        .border_color(pal.border)
                        .bg(hex_to_hsla(colors.hex_field(index)).unwrap_or(pal.border)),
                )
                .child(styled_input(Input::new(&self.custom_color_inputs[index]), pal).w(px(90.0)))
        };
        let labeled = |label: &'static str, index: usize| {
            h_flex()
                .w(px(150.0))
                .gap_1()
                .items_center()
                .child(
                    div()
                        .w(px(32.0))
                        .text_sm()
                        .text_color(pal.muted)
                        .child(label),
                )
                .child(cell(index))
        };
        let mut grid = v_flex().gap_1().child(
            h_flex()
                .gap_3()
                .child(labeled("前景", 0))
                .child(labeled("背景", 1)),
        );
        for i in 0..8 {
            grid = grid.child(
                h_flex()
                    .gap_3()
                    .child(labeled(ANSI_COLOR_NAMES[i], CUSTOM_ANSI_BASE + i))
                    .child(labeled(ANSI_COLOR_NAMES[i], CUSTOM_BRIGHT_BASE + i)),
            );
        }
        v_flex()
            .gap_2()
            .rounded(px(6.0))
            .border_1()
            .border_color(pal.border)
            .p_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(pal.text)
                            .child("自定义 16 色（左列普通 · 右列明亮）"),
                    )
                    .child(
                        Button::new("terminal-custom-reset")
                            .label("恢复 One Dark")
                            .secondary()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_custom_colors(window, cx);
                            })),
                    ),
            )
            .child(grid)
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child("输入 #RGB 或 #RRGGBB，回车前即生效；格式不合法时保持上一个有效颜色。"),
            )
            .into_any_element()
    }

    /// 「会话」分页：断线自动重连与连接超时，改动即时生效并写入凭据库。
    fn render_session_tab(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let auto = cx.entity();
        v_flex()
            .id("settings-tab-session")
            .gap_3()
            .child(div().text_sm().text_color(pal.muted).child(
                "连接意外中断（断网、服务端掉线）后自动重建 SSH 会话；远端主动 exit 不触发自动重连。",
            ))
            .child(
                Checkbox::new("session-auto-reconnect")
                    .label("断线后自动重连")
                    .checked(self.session_prefs.auto_reconnect)
                    .on_change(move |value, _window, cx| {
                        auto.update(cx, |this, cx| this.set_auto_reconnect(value, cx));
                    }),
            )
            .child(div().text_xs().text_color(pal.muted).child(format!(
                "首次连接就失败（地址写错、端口不通）不自动重试；断线后最多连续重试 \
                 {AUTO_RECONNECT_MAX} 次，退避 1 / 2 / 5 / 10 / 20 秒，\
                 会话稳定运行 30 秒后重新计数。等待期间可在横幅里取消或按回车立即重连。"
            )))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(72.0))
                            .text_sm()
                            .text_color(pal.muted)
                            .child("连接超时"),
                    )
                    .child(stepper(
                        "session-timeout-minus",
                        "−",
                        cx.listener(|this, _, _, cx| this.nudge_connect_timeout(-1, cx)),
                    ))
                    .child(
                        div()
                            .w(px(64.0))
                            .text_center()
                            .text_sm()
                            .flex_shrink_0()
                            .child(format!("{} 秒", self.session_prefs.connect_timeout)),
                    )
                    .child(stepper(
                        "session-timeout-plus",
                        "+",
                        cx.listener(|this, _, _, cx| this.nudge_connect_timeout(1, cx)),
                    )),
            )
            .child(div().text_xs().text_color(pal.muted).child(
                "超时覆盖 DNS、TCP、握手、认证与开启 shell 的全过程；超时即结束本次尝试并报「连接超时」。",
            ))
            .into_any_element()
    }

    /// 设置「终端」分页里的本机 shell 一行：扫描结果计数 + 一键扫描按钮。
    fn render_local_shell_row(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let count = self.local_shells.len();
        let scanning = cx.entity();
        h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .w(px(72.0))
                    .text_sm()
                    .text_color(pal.muted)
                    .child("本机 shell"),
            )
            .child(div().text_sm().child(if count == 0 {
                "未扫描到".to_string()
            } else {
                format!("已扫到 {count} 个")
            }))
            .child(
                Button::new("scan-local-shells")
                    .label(if self.scanning_shells {
                        "扫描中…"
                    } else {
                        "一键扫描"
                    })
                    .secondary()
                    .outline()
                    .on_click(move |_, _, cx| {
                        scanning.update(cx, |this, cx| this.scan_local_shells(cx));
                    }),
            )
            .into_any_element()
    }

    /// 窗口级浮层：删除二次确认、主机表单、分组新建/重命名等居中模态。
    /// （主机右键菜单不在这里——它走框架的独立图层，见 `context_menu`。）
    fn render_overlay(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        // 解锁 / 主密码弹窗优先于一切浮层（库未解锁时其余操作无意义）。
        if self.unlock.is_some() {
            return self.render_unlock_dialog(cx);
        }
        if self.settings.is_some() {
            return self.render_settings_dialog(cx);
        }
        if self.transfer.is_some() {
            return self.render_transfer_dialog(cx);
        }
        if self.forward.is_some() {
            return self.render_forward_dialog(cx);
        }
        // 删除二次确认：半透明遮罩 + 居中卡片，点遮罩或"取消"关闭。
        if let Some(index) = self.confirm_delete {
            let label = self
                .hosts
                .get(index)
                .map(|host| {
                    if host.name.is_empty() {
                        host.addr()
                    } else {
                        host.name.clone()
                    }
                })
                .unwrap_or_else(|| "该主机".to_string());
            return v_flex()
                .id("delete-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .items_center()
                .justify_center()
                .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.cancel_delete(cx);
                    }),
                )
                .child(
                    v_flex()
                        .id("delete-dialog")
                        .w(px(340.0))
                        .gap_3()
                        .p_4()
                        .rounded(px(8.0))
                        .bg(pal.card_bg)
                        .border_1()
                        .border_color(pal.border)
                        .child(
                            div()
                                .text_base()
                                .font_weight(rgpui::FontWeight(600.0))
                                .child(format!("删除主机 {label}？")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(pal.muted)
                                .child("若该主机已保存密码/口令，将一并从加密库中清除。"),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(Button::new("cancel-delete").label("取消").ghost().on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.cancel_delete(cx);
                                    }),
                                ))
                                .child(
                                    Button::new("confirm-delete")
                                        .label("删除")
                                        .danger()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_delete_now(cx);
                                        })),
                                ),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| {
                                // 阻止卡片内点击冒泡到遮罩（否则点对话框也会关闭）。
                                cx.stop_propagation();
                            }),
                        ),
                )
                .into_any_element();
        }
        // 新建/编辑主机：居中模态弹窗（不再挤占侧边栏窄面板）。
        if self.new_host.is_some() {
            return self.render_form_dialog(cx);
        }
        // 分组新建 / 重命名：同名模态，宽度固定，避免列表内联编辑误丢内容。
        if self.group_prompt.is_some() {
            return self.render_group_prompt(cx);
        }
        if self.shell_prompt.is_some() {
            return self.render_shell_prompt(cx);
        }
        div().into_any_element()
    }

    /// 侧边栏右键菜单内容，按目标（空白处 / 分组 / 主机行）给出不同条目。
    /// 顶部常驻的「+ 新建主机」大按钮已取消，新建一律走这里或「会话」菜单。
    ///
    /// 走框架的 `PopupMenu`（`deferred + anchored` 独立图层）而不是手搭浮层：
    /// 手搭的绝对定位浮层留在普通图层里，会被后画的兄弟节点压住 hitbox，
    /// 菜单项收不到 mouse-up，点下去只关菜单不触发。
    fn context_menu(
        target: MenuTarget,
        groups: Vec<SharedString>,
        current_group: Option<SharedString>,
        entity: Entity<RuLinkApp>,
        menu: PopupMenu,
    ) -> PopupMenu {
        let menu = menu.min_w(px(168.0));
        // 「新建主机…」落在右键目标所在的分组：分组上直接进该组，
        // 主机上沿用该主机的组，空白处则是未分组。
        let target_group = match &target {
            MenuTarget::Group(name) if name.as_str() != UNGROUPED => Some(name.clone()),
            MenuTarget::Host(_) => current_group.clone(),
            _ => None,
        };
        let menu = match &target {
            MenuTarget::Host(index) => {
                let index = *index;
                menu.item(Self::menu_item(
                    entity.clone(),
                    "编辑",
                    None,
                    false,
                    false,
                    move |this, window, cx| {
                        this.open_form_at(Some(index), None, window, cx);
                    },
                ))
                .separator()
                .item(Self::menu_item(
                    entity.clone(),
                    "上移一位",
                    None,
                    false,
                    false,
                    move |this, _, cx| this.reorder_host(index, ReorderAction::Up, cx),
                ))
                .item(Self::menu_item(
                    entity.clone(),
                    "下移一位",
                    None,
                    false,
                    false,
                    move |this, _, cx| this.reorder_host(index, ReorderAction::Down, cx),
                ))
                .item(Self::menu_item(
                    entity.clone(),
                    "移到组内最前",
                    None,
                    false,
                    false,
                    move |this, _, cx| this.reorder_host(index, ReorderAction::Top, cx),
                ))
                .separator()
            }
            _ => menu,
        };
        let menu = menu
            .item(Self::menu_item(
                entity.clone(),
                "新建主机…",
                None,
                false,
                false,
                move |this, window, cx| {
                    this.open_form_in_group(target_group.clone(), window, cx);
                },
            ))
            .item(Self::menu_item(
                entity.clone(),
                "本地终端…",
                Some(IconName::SquareTerminal),
                false,
                false,
                |this, _, cx| {
                    this.open_local_terminal(None, cx);
                },
            ))
            .item(Self::menu_item(
                entity.clone(),
                "新建分组…",
                Some(IconName::Folder),
                false,
                false,
                |this, window, cx| {
                    this.open_group_prompt(None, window, cx);
                },
            ));
        let menu = match &target {
            MenuTarget::Group(name) if name.as_str() != UNGROUPED => {
                let renaming = name.clone();
                let deleting = name.clone();
                menu.separator()
                    .item(Self::menu_item(
                        entity.clone(),
                        "重命名分组…",
                        None,
                        false,
                        false,
                        move |this, window, cx| {
                            this.open_group_prompt(Some(renaming.clone()), window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        entity.clone(),
                        "删除分组",
                        None,
                        true,
                        false,
                        move |this, _, cx| {
                            this.delete_group(&deleting, cx);
                        },
                    ))
            }
            _ => menu,
        };
        let menu = match &target {
            MenuTarget::Host(index) => {
                let index = *index;
                let current = current_group.clone();
                let mut menu = menu.separator().label("移动到分组").item(Self::menu_item(
                    entity.clone(),
                    UNGROUPED,
                    None,
                    false,
                    current.is_none(),
                    move |this, _, cx| {
                        this.move_host(index, None, cx);
                    },
                ));
                for name in groups {
                    let group = name.clone();
                    let checked = current.as_ref() == Some(&name);
                    menu = menu.item(Self::menu_item(
                        entity.clone(),
                        name,
                        Some(IconName::Folder),
                        false,
                        checked,
                        move |this, _, cx| {
                            this.move_host(index, Some(group.clone()), cx);
                        },
                    ));
                }
                menu
            }
            _ => menu,
        };
        // 空白处额外给一个批量入口（主机行菜单已经够长，不重复放）。
        let menu = match &target {
            MenuTarget::Blank => menu.separator().item(Self::menu_item(
                entity.clone(),
                "导入 ~/.ssh/config…",
                None,
                false,
                false,
                |this, _, cx| this.import_ssh_config(cx),
            )),
            _ => menu,
        };
        match target {
            MenuTarget::Host(index) => menu.separator().item(Self::menu_item(
                entity,
                "删除",
                Some(IconName::Delete),
                true,
                false,
                move |this, _, cx| {
                    this.ask_delete(index, cx);
                },
            )),
            _ => menu,
        }
    }

    /// 右键菜单的一项：回调要求 `Send + Sync`，故只捕获实体与标签，不捕获 `&self`。
    fn menu_item(
        entity: Entity<RuLinkApp>,
        label: impl Into<SharedString>,
        icon: Option<IconName>,
        danger: bool,
        checked: bool,
        run: impl Fn(&mut RuLinkApp, &mut Window, &mut Context<RuLinkApp>) + Send + Sync + 'static,
    ) -> PopupMenuItem {
        let label = label.into();
        let mut item = PopupMenuItem::new(label).danger(danger).checked(checked);
        if let Some(icon) = icon {
            item = item.icon(icon);
        }
        item.on_click(move |_, window, cx| {
            let _ = entity.update(cx, |this, cx| run(this, window, cx));
        })
    }

    /// 主机行图标：按认证方式区分，一眼看出连这台机器要什么凭据。
    fn host_icon(auth: AuthMethod) -> IconName {
        match auth {
            AuthMethod::Agent => IconName::Network,
            AuthMethod::IdentityFile => IconName::File,
            AuthMethod::Password => IconName::CircleUser,
        }
    }

    /// 列表行的通用外观：透明底 + 悬停高亮（树式，不再是每张卡片一块底），
    /// 左侧留出的缩进由 `indent` 控制，分组下的主机比文件夹行多缩一档。
    fn row_base(id: impl Into<SharedString>, pal: &Palette, indent: Pixels) -> Stateful<Div> {
        h_flex()
            .id(id.into())
            .w_full()
            .pl(indent)
            .pr_2()
            .py_1()
            .gap_2()
            .items_center()
            .rounded(px(5.0))
            .hover(|style| style.bg(pal.card_hover))
            .cursor_pointer()
    }

    /// 内置「本地终端」分组：父行折叠/展开，子行是扫描到的每个本机 shell，点一下直接开对应终端。
    /// 它不是凭据库里的主机，所以没有 ★ / 编辑 / 删除，也不参与分组与收藏排序；
    /// 搜索时按「本地终端」与各 shell 名匹配，命中即展开。
    /// 子行图标取该 exe 的系统默认图标（`SHGetFileInfoW`），取不到回退字形。
    fn render_local_terminal_rows(
        &self,
        pal: &Palette,
        query: &str,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let entity = cx.entity();
        let searching = !query.is_empty();
        let hit = query.is_empty()
            || "本地终端".contains(query)
            || self
                .local_shells
                .iter()
                .any(|profile| self.shell_matches(profile, query));
        if !hit {
            return Vec::new();
        }
        let count = self.local_shells.len();
        let collapsed = !searching && self.local_group_collapsed;
        let parent = Self::row_base("local-terminal-row", pal, px(2.0))
            .font_weight(rgpui::FontWeight(600.0))
            .on_click(cx.listener(|this, _, _, cx| {
                this.local_group_collapsed = !this.local_group_collapsed;
                cx.notify();
            }))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(pal.muted),
            )
            .child(
                Icon::new(IconName::SquareTerminal)
                    .small()
                    .text_color(pal.link),
            )
            .child(div().flex_1().min_w_0().text_ellipsis().child("本地终端"))
            .when(count > 0, |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(count.to_string()),
                )
            })
            .context_menu(move |menu, _, _| {
                let scanning = entity.clone();
                menu.item(
                    PopupMenuItem::new("一键扫描本机 shell…").on_click(move |_, _, cx| {
                        scanning.update(cx, |this, cx| this.scan_local_shells(cx));
                    }),
                )
                .item(PopupMenuItem::new("新建主机…").on_click({
                    let entity = entity.clone();
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.open_form(window, cx));
                    }
                }))
                .item(PopupMenuItem::new("新建分组…").on_click({
                    let entity = entity.clone();
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.open_group_prompt(None, window, cx));
                    }
                }))
            })
            .into_any_element();
        if collapsed {
            return vec![parent];
        }
        // 下标必须是 `local_shells` 里的真实下标：先 enumerate 再过滤，
        // 否则搜索态下点行会按过滤后的序号打开另一台 shell。
        let rows: Vec<AnyElement> = self
            .local_shells
            .iter()
            .enumerate()
            .filter(|(_, profile)| self.shell_matches(profile, query))
            .map(|(index, profile)| self.render_shell_row(index, profile, pal, cx))
            .collect();
        std::iter::once(parent).chain(rows).collect()
    }

    /// 「本地终端」分组下的一台 shell：系统图标 + 展示名，点开即开该 shell 的终端标签。
    fn render_shell_row(
        &self,
        index: usize,
        profile: &ShellProfile,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let name = self.shell_name(profile);
        let profile_id = profile.id();
        let renamed = self.local_shell_prefs.name_of(&profile_id).is_some();
        let custom_cwd = self.local_shell_prefs.cwd_of(&profile_id).is_some();
        let custom_args = !self.local_shell_prefs.args_of(&profile_id).is_empty();
        let custom_env = !self.local_shell_prefs.env_of(&profile_id).is_empty();
        let system_name = profile.name.clone();
        let icon: AnyElement = match self.shell_icons.get(&profile.program).cloned() {
            Some(image) => img(image)
                .size_4()
                .flex_shrink_0()
                .text_color(pal.text)
                .into_any_element(),
            None => Icon::new(IconName::SquareTerminal)
                .small()
                .text_color(pal.muted)
                .into_any_element(),
        };
        Self::row_base(
            SharedString::from(format!("local-shell-row-{index}")),
            pal,
            px(22.0),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            let chosen = this.local_shells.get(index).cloned();
            this.open_local_terminal(chosen.as_ref(), cx);
        }))
        .child(icon)
        .child(div().flex_1().min_w_0().text_ellipsis().child(name.clone()))
        .context_menu(move |menu, _, _| {
            let opening = entity.clone();
            let menu = menu.item(PopupMenuItem::new(format!("打开 {name}")).on_click(
                move |_, _, cx| {
                    opening.update(cx, |this, cx| {
                        let chosen = this.local_shells.get(index).cloned();
                        this.open_local_terminal(chosen.as_ref(), cx);
                    });
                },
            ));
            let renaming = entity.clone();
            let menu = menu.item(
                PopupMenuItem::new("重命名…").on_click(move |_, window, cx| {
                    renaming.update(cx, |this, cx| {
                        let Some(profile) = this.local_shells.get(index).cloned() else {
                            return;
                        };
                        this.open_shell_prompt(&profile, window, cx);
                    });
                }),
            );
            let choosing_cwd = entity.clone();
            let menu = menu.item(
                PopupMenuItem::new("起始目录…").on_click(move |_, window, cx| {
                    choosing_cwd.update(cx, |this, cx| {
                        let Some(profile) = this.local_shells.get(index).cloned() else {
                            return;
                        };
                        this.open_shell_cwd_prompt(&profile, window, cx);
                    });
                }),
            );
            let editing_args = entity.clone();
            let menu = menu.item(
                PopupMenuItem::new("启动参数…").on_click(move |_, window, cx| {
                    editing_args.update(cx, |this, cx| {
                        let Some(profile) = this.local_shells.get(index).cloned() else {
                            return;
                        };
                        this.open_shell_args_prompt(&profile, window, cx);
                    });
                }),
            );
            let editing_env = entity.clone();
            let menu = menu.item(
                PopupMenuItem::new("环境变量…").on_click(move |_, window, cx| {
                    editing_env.update(cx, |this, cx| {
                        let Some(profile) = this.local_shells.get(index).cloned() else {
                            return;
                        };
                        this.open_shell_env_prompt(&profile, window, cx);
                    });
                }),
            );
            let menu = if custom_cwd {
                let resetting = entity.clone();
                menu.item(
                    PopupMenuItem::new("恢复默认起始目录").on_click(move |_, _, cx| {
                        resetting.update(cx, |this, cx| {
                            let Some(profile) = this.local_shells.get(index).cloned() else {
                                return;
                            };
                            this.reset_shell_cwd(&profile, cx);
                        });
                    }),
                )
            } else {
                menu
            };
            let menu = if custom_args {
                let resetting = entity.clone();
                menu.item(
                    PopupMenuItem::new("清除启动参数").on_click(move |_, _, cx| {
                        resetting.update(cx, |this, cx| {
                            let Some(profile) = this.local_shells.get(index).cloned() else {
                                return;
                            };
                            this.reset_shell_args(&profile, cx);
                        });
                    }),
                )
            } else {
                menu
            };
            let menu = if custom_env {
                let resetting = entity.clone();
                menu.item(
                    PopupMenuItem::new("清除环境变量").on_click(move |_, _, cx| {
                        resetting.update(cx, |this, cx| {
                            let Some(profile) = this.local_shells.get(index).cloned() else {
                                return;
                            };
                            this.reset_shell_env(&profile, cx);
                        });
                    }),
                )
            } else {
                menu
            };
            if renamed {
                let resetting = entity.clone();
                menu.item(
                    PopupMenuItem::new(format!("恢复默认名称（{system_name}）")).on_click(
                        move |_, _, cx| {
                            resetting.update(cx, |this, cx| {
                                let Some(profile) = this.local_shells.get(index).cloned() else {
                                    return;
                                };
                                this.reset_shell_name(&profile, cx);
                            });
                        },
                    ),
                )
            } else {
                menu
            }
        })
        .into_any_element()
    }

    /// 分组（文件夹）行：折叠三角 + 文件夹图标 + 组名 + 数量；
    /// 点整行折叠/展开，右键给出该组的分组管理与新建入口。
    fn render_group_row(
        &self,
        name: &str,
        count: usize,
        collapsed: bool,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let groups: Vec<SharedString> = self
            .sidebar
            .names()
            .map(|n| SharedString::from(n.to_string()))
            .collect();
        let toggle_name = SharedString::from(name.to_string());
        let menu_name = toggle_name.clone();
        Self::row_base(
            SharedString::from(format!("group-row-{name}")),
            pal,
            px(2.0),
        )
        .font_weight(rgpui::FontWeight(600.0))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_group(&toggle_name, cx);
        }))
        .child(
            Icon::new(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .xsmall()
            .text_color(pal.muted),
        )
        .child(
            Icon::new(if collapsed {
                IconName::FolderClosed
            } else {
                IconName::FolderOpen
            })
            .small()
            .text_color(pal.link),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_ellipsis()
                .child(name.to_string()),
        )
        .when(count > 0, |this| {
            this.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(count.to_string()),
            )
        })
        .context_menu(move |menu, _, _| {
            Self::context_menu(
                MenuTarget::Group(menu_name.clone()),
                groups.clone(),
                None,
                entity.clone(),
                menu,
            )
        })
        .into_any_element()
    }

    /// 一台主机：认证方式图标 + 展示名 + user@host:port + ★。
    fn render_host_row(&self, index: usize, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let host = &self.hosts[index];
        let entity = cx.entity();
        let groups: Vec<SharedString> = self
            .sidebar
            .names()
            .map(|n| SharedString::from(n.to_string()))
            .collect();
        let current = self
            .sidebar
            .group_of(host.id)
            .map(|name| SharedString::from(name.to_string()));
        let fav_glyph: &'static str = if host.favorite { "★" } else { "☆" };
        // 没填展示名时标题直接用地址，避免整行只剩一行灰字。
        let title = if host.name.is_empty() {
            host.addr()
        } else {
            host.name.clone()
        };
        Self::row_base(
            SharedString::from(format!("host-row-{index}")),
            pal,
            px(22.0),
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.click_host(index, window, cx);
        }))
        .child(
            Icon::new(Self::host_icon(host.auth))
                .small()
                .text_color(if host.favorite { pal.accent } else { pal.muted }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(
                    div()
                        .text_ellipsis()
                        .font_weight(rgpui::FontWeight(500.0))
                        .child(title),
                )
                .child(
                    div()
                        .text_ellipsis()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(host.addr()),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("host-fav-{index}")))
                .flex_shrink_0()
                .text_sm()
                .px_1()
                .text_color(if host.favorite { pal.accent } else { pal.muted })
                .child(fav_glyph)
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_favorite(index, cx))),
        )
        .context_menu(move |menu, _, _| {
            Self::context_menu(
                MenuTarget::Host(index),
                groups.clone(),
                current.clone(),
                entity.clone(),
                menu,
            )
        })
        .into_any_element()
    }

    /// 主机分组树：内置「本地终端」→ 各自定义分组 → 未分组 → 空白区。
    /// 分组为空也照样显示（和参考客户端一致：先建组，再往里挪主机）。
    fn render_host_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        // 按展示名 / user@host:port 子串过滤；保留真实下标供点击/删除定位。
        let mut matched: Vec<usize> = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, host)| {
                query.is_empty()
                    || host.name.to_lowercase().contains(&query)
                    || host.addr().to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        // 收藏主机置顶；同桶内按手动排序，未登记的新主机稳定留在后面。
        matched.sort_by_key(|&index| {
            let host = &self.hosts[index];
            (!host.favorite, self.sidebar.rank_of(host.id))
        });
        // 分桶：先按偏好里的组顺序建桶，末尾恒有「未分组」。
        let mut buckets: Vec<(String, Vec<usize>)> = self
            .sidebar
            .names()
            .map(|name| (name.to_string(), Vec::new()))
            .chain(std::iter::once((UNGROUPED.to_string(), Vec::new())))
            .collect();
        for index in matched {
            let group = self
                .sidebar
                .group_of(self.hosts[index].id)
                .unwrap_or(UNGROUPED)
                .to_string();
            match buckets.iter_mut().find(|(name, _)| *name == group) {
                Some((_, rows)) => rows.push(index),
                // 归属指向一个已被删掉的组：退回未分组（group_of 已过滤，这里兜底）。
                None => {
                    if let Some((_, rows)) = buckets.last_mut() {
                        rows.push(index);
                    }
                }
            }
        }
        // 内置「本地终端」条目恒在最前（不入库，故也不受删除/分组/收藏影响）。
        let local_rows = self.render_local_terminal_rows(&pal, &query, cx);
        // 列表末尾的空白区：右键给出「仅新建」的菜单。它是行的兄弟节点而非祖先，
        // 否则一次右键会同时命中两层菜单，弹出两个重叠的浮层。
        let blank = {
            let entity = cx.entity();
            let groups: Vec<SharedString> = self
                .sidebar
                .names()
                .map(|n| SharedString::from(n.to_string()))
                .collect();
            div()
                .id("sidebar-blank")
                .w_full()
                .min_h(px(80.0))
                .flex_1()
                .context_menu(move |menu, _, _| {
                    Self::context_menu(
                        MenuTarget::Blank,
                        groups.clone(),
                        None,
                        entity.clone(),
                        menu,
                    )
                })
        };
        let searching = !query.is_empty();
        // 搜索命中本机 shell 时，主机没有命中也不该再叠一句"无匹配主机"（看着像整体没结果）。
        let local_hit = !local_rows.is_empty();
        let mut list = v_flex().gap_px().min_h_full().children(local_rows);
        for (name, rows) in &buckets {
            // 「未分组」只是兜底桶：没有未分组主机时根本不渲染——否则刚建完分组、一台主机都没加时，
            // 它会和空的新组并排出现，看着像凭空多了一个组。自定义空组保留（那是有意建的容器）。
            // 搜索态下空组一律隐藏、折叠强制展开，否则命中的主机看不见。
            if rows.is_empty() && (searching || name == UNGROUPED) {
                continue;
            }
            let collapsed = !searching && self.sidebar.collapsed(name);
            list = list.child(self.render_group_row(name, rows.len(), collapsed, &pal, cx));
            if collapsed {
                continue;
            }
            list = list.children(
                rows.iter()
                    .map(|&index| self.render_host_row(index, &pal, cx)),
            );
        }
        // 树顶已有本机 shell 列表，首启动不再塞"暂无主机"那行大字（空着比提示好看）；
        // 只在搜索没命中时给一行反馈。
        if searching && !local_hit {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(pal.muted)
                    .child("无匹配主机"),
            );
        }
        list.child(blank).into_any_element()
    }

    /// 新建/编辑主机：居中模态弹窗。遮罩只挡交互、点击不关闭（防误丢已填内容），
    /// 关闭仅走"取消"按钮或标题栏 ×。
    fn render_form_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(form) = self.new_host.as_ref() else {
            return div().into_any_element();
        };
        let methods = [
            AuthMethod::Agent,
            AuthMethod::IdentityFile,
            AuthMethod::Password,
        ];
        let selected = form.auth;
        let save_checked = form.save_secret;
        let editing = form.editing.is_some();
        v_flex()
            .id("form-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .items_center()
            .justify_center()
            .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(
                v_flex()
                    .id("host-form")
                    .w(px(440.0))
                    .gap_3()
                    .p_5()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .shadow_md()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(rgpui::FontWeight(600.0))
                                    .child(if editing {
                                        "编辑主机"
                                    } else {
                                        "新建主机"
                                    }),
                            )
                            .child(
                                Button::new("close-form")
                                    .label("×")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
                            ),
                    )
                    .child(labeled_field("展示名", &form.name, &pal))
                    .child(labeled_field("主机地址", &form.host, &pal))
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .child(labeled_field("端口", &form.port, &pal)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .child(labeled_field("用户名", &form.user, &pal)),
                            ),
                    )
                    .child({
                        // 分组选择：新建时默认未分组（从分组右键进入则预选该组），
                        // 编辑时预填原归属；保存即落到这里选中的组。
                        let mut options: Vec<SharedString> = vec![SharedString::from(UNGROUPED)];
                        options.extend(
                            self.sidebar
                                .names()
                                .map(|name| SharedString::from(name.to_string())),
                        );
                        let selected = form
                            .group
                            .as_ref()
                            .and_then(|group| options.iter().position(|option| option == group))
                            .or(Some(0));
                        let change_options = options.clone();
                        let entity = cx.entity();
                        v_flex().gap_1().child(div().text_sm().child("分组")).child(
                            Select::new(options)
                                .id("host-group")
                                .selected(selected)
                                .w_full()
                                .on_change(move |index, _name, _window, cx| {
                                    let group = if index == 0 {
                                        None
                                    } else {
                                        change_options.get(index).cloned()
                                    };
                                    entity.update(cx, |this, cx| {
                                        if let Some(form) = this.new_host.as_mut() {
                                            form.group = group;
                                            cx.notify();
                                        }
                                    });
                                }),
                        )
                    })
                    .child({
                        // 跳板机选择：只列出可作为跳板的主机（排除自己与已挂跳板机的主机，
                        // 避免自环与多级链）。降级模式没有稳定的主机 id，只给「直连」。
                        let self_id = form
                            .editing
                            .and_then(|index| self.hosts.get(index))
                            .map(|host| host.id);
                        let mut options: Vec<SharedString> =
                            vec![SharedString::from("直连（无跳板机）")];
                        let mut ids: Vec<Option<i64>> = vec![None];
                        if self.vault.is_some() {
                            for host in &self.hosts {
                                if Some(host.id) == self_id || host.proxy_jump.is_some() {
                                    continue;
                                }
                                let label = if host.name.trim().is_empty() {
                                    host.addr()
                                } else {
                                    host.name.clone()
                                };
                                ids.push(Some(host.id));
                                options.push(SharedString::from(format!(
                                    "{label}（{}）",
                                    host.addr()
                                )));
                            }
                        }
                        let selected = form
                            .jump
                            .and_then(|id| ids.iter().position(|item| *item == Some(id)))
                            .or(Some(0));
                        let change_ids = ids.clone();
                        let entity = cx.entity();
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child("跳板机"))
                            .child(
                                Select::new(options)
                                    .id("host-jump")
                                    .selected(selected)
                                    .w_full()
                                    .on_change(move |index, _name, _window, cx| {
                                        let jump = change_ids.get(index).copied().flatten();
                                        entity.update(cx, |this, cx| {
                                            if let Some(form) = this.new_host.as_mut() {
                                                form.jump = jump;
                                                cx.notify();
                                            }
                                        });
                                    }),
                            )
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child("认证方式"))
                            .child(h_flex().gap_2().children(methods.into_iter().map(|method| {
                                let auth_id =
                                    SharedString::from(format!("auth-{}", auth_label(method)));
                                let is_selected = method == selected;
                                div()
                                    .id(auth_id)
                                    .px_3()
                                    .py_1()
                                    .rounded(px(5.0))
                                    .text_sm()
                                    .cursor_pointer()
                                    .bg(if is_selected {
                                        pal.accent
                                    } else {
                                        pal.segment_bg
                                    })
                                    // 选中段文字用主题的 primary_foreground（与主按钮同规则），
                                    // 否则强调色底上会沿用默认文字色导致看不清。
                                    .text_color(if is_selected {
                                        cx.theme().primary_foreground
                                    } else {
                                        pal.text
                                    })
                                    .when(is_selected, |this| this.font_weight(FontWeight::BOLD))
                                    .child(auth_label(method))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(form) = this.new_host.as_mut() {
                                            form.auth = method;
                                        }
                                        cx.notify();
                                    }))
                            }))),
                    )
                    // 私钥区块常驻布局：非私钥时用 invisible 保留占位（隐藏绘制与事件），
                    // 切换认证方式弹窗高度不跳变。
                    .child(
                        v_flex()
                            .gap_3()
                            .when(selected != AuthMethod::IdentityFile, |this| {
                                this.invisible()
                            })
                            // 私钥来源：本地文件 / 内嵌加密库。两段切换不改变弹窗高度
                            // （下方行数一致：路径行 ↔ 导入行）。
                            .child({
                                let inline_available = self.vault.is_some();
                                let source_id =
                                    |label: &str| SharedString::from(format!("key-source-{label}"));
                                let seg = |label: &'static str, inline: bool| {
                                    let is_selected = form.key_inline == inline;
                                    let enabled = !inline || inline_available;
                                    div()
                                        .id(source_id(label))
                                        .px_3()
                                        .py_1()
                                        .rounded(px(5.0))
                                        .text_sm()
                                        .cursor_pointer()
                                        .bg(if is_selected {
                                            pal.accent
                                        } else {
                                            pal.segment_bg
                                        })
                                        .text_color(if is_selected {
                                            cx.theme().primary_foreground
                                        } else if enabled {
                                            pal.text
                                        } else {
                                            pal.muted
                                        })
                                        .when(is_selected, |this| {
                                            this.font_weight(FontWeight::BOLD)
                                        })
                                        .child(label)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if inline && this.vault.is_none() {
                                                this.status =
                                                    "内嵌私钥需要加密凭据库（当前不可用）".into();
                                                cx.notify();
                                                return;
                                            }
                                            if let Some(form) = this.new_host.as_mut() {
                                                form.key_inline = inline;
                                            }
                                            cx.notify();
                                        }))
                                };
                                v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child("私钥来源"))
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(seg("本地文件", false))
                                            .child(seg("内嵌到加密库", true)),
                                    )
                            })
                            .when(!form.key_inline, |this| {
                                this.child(
                                    h_flex()
                                        .gap_2()
                                        .items_end()
                                        .child(div().flex_1().child(labeled_field(
                                            "私钥路径",
                                            &form.key_path,
                                            &pal,
                                        )))
                                        .child(
                                            Button::new("browse-key")
                                                .label("浏览…")
                                                .ghost()
                                                .flex_shrink_0()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.browse_key_path(cx);
                                                })),
                                        ),
                                )
                            })
                            .when(form.key_inline, |this| {
                                // 已导入的私钥只显示文件名与字节数，界面里绝不出现内容。
                                let note = match (&form.key_label, &form.key_data) {
                                    (Some(name), Some(pem)) => {
                                        format!("待保存：{name}（{} 字节）", pem.len())
                                    }
                                    _ if form.editing.is_some() => {
                                        "已内嵌在加密库（重新导入可替换）".to_string()
                                    }
                                    _ => "尚未导入私钥".to_string(),
                                };
                                this.child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_sm()
                                                .text_color(pal.muted)
                                                .truncate()
                                                .child(note),
                                        )
                                        .child(
                                            Button::new("import-key")
                                                .label("导入私钥…")
                                                .ghost()
                                                .flex_shrink_0()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.import_inline_key(cx);
                                                })),
                                        ),
                                )
                            })
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child("私钥口令"))
                                    .child(styled_input(
                                        Input::new(&form.passphrase)
                                            .content_type(InputContentType::Password),
                                        &pal,
                                    )),
                            )
                            .child(
                                h_flex()
                                    .id("save-passphrase")
                                    .gap_2()
                                    .items_center()
                                    .when(self.vault.is_some(), |this| {
                                        this.cursor_pointer().on_click(cx.listener(
                                            |this, _, _, cx| {
                                                if let Some(form) = this.new_host.as_mut() {
                                                    form.save_secret = !form.save_secret;
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                    })
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(if save_checked {
                                                pal.text
                                            } else {
                                                pal.muted
                                            })
                                            .child(if save_checked { "☑" } else { "☐" }),
                                    )
                                    .child(div().text_sm().text_color(pal.muted).child(
                                        if self.vault.is_some() {
                                            "保存口令（加密存入本地凭据库，默认仅本次内存持有）"
                                        } else {
                                            "加密库不可用，口令仅本次内存持有"
                                        },
                                    )),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("save-host")
                                    .label(if editing { "保存修改" } else { "保存" })
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| this.save_form(cx))),
                            )
                            .child(
                                Button::new("cancel-host")
                                    .label("取消")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 分组新建 / 重命名弹窗：与主机表单同款居中模态。
    /// 遮罩只挡交互不关闭（防止误丢已输入的名称），关闭走"取消"或标题栏 ×。
    fn render_group_prompt(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(prompt) = self.group_prompt.as_ref() else {
            return div().into_any_element();
        };
        let renaming = prompt.renaming.clone();
        let title = match &renaming {
            Some(old) => format!("重命名分组：{old}"),
            None => "新建分组".to_string(),
        };
        v_flex()
            .id("group-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .items_center()
            .justify_center()
            .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(
                v_flex()
                    .id("group-form")
                    .w(px(360.0))
                    .gap_3()
                    .p_5()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .shadow_md()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(rgpui::FontWeight(600.0))
                                    .child(title),
                            )
                            .child(Button::new("close-group").label("×").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.cancel_group_prompt(cx);
                                }),
                            )),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child("分组名称"))
                            .child(styled_input(Input::new(&prompt.name), &pal)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child("分组只影响列表的组织方式，不改动主机本身。"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("commit-group")
                                    .label(if renaming.is_some() {
                                        "重命名"
                                    } else {
                                        "创建"
                                    })
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.commit_group_prompt(cx);
                                    })),
                            )
                            .child(Button::new("cancel-group").label("取消").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.cancel_group_prompt(cx);
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }

    /// 本地 shell 重命名弹窗：与分组重命名同款居中模态。
    fn render_shell_prompt(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(prompt) = self.shell_prompt.as_ref() else {
            return div().into_any_element();
        };
        let default_text = prompt.default_text.clone();
        let (title, field_label, hint) = match prompt.kind {
            ShellPromptKind::Rename => (
                "重命名本地终端",
                "显示名称",
                format!(
                    "只改侧边栏与标签上的叫法，不影响启动的 shell 本身；留空恢复默认（{default_text}）。"
                ),
            ),
            ShellPromptKind::WorkingDirectory => (
                "起始目录",
                "工作目录",
                format!(
                    "新会话在此目录下启动；留空或保持默认（{default_text}）时用进程当前目录。\
                     目录不存在时不会保存。"
                ),
            ),
            ShellPromptKind::Arguments => (
                "启动参数",
                "参数",
                "附加在 shell 可执行文件后的参数，空格分隔，含空格的路径用双引号包住；\
                 留空＝不加参数。WSL 的 -d 发行版由应用自动拼接，不必重复填写。"
                    .to_string(),
            ),
            ShellPromptKind::Environment => (
                "环境变量",
                "变量（每行一个 KEY=VALUE）",
                "空行与 # 开头的注释忽略；值里可以有 =。留空＝不注入额外变量。\
                 回车换行，点「保存」提交。"
                    .to_string(),
            ),
        };
        let width = if prompt.kind.multi_line() { 460.0 } else { 380.0 };
        let input_row = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(styled_input(Input::new(&prompt.input), &pal)),
            )
            .when(prompt.kind == ShellPromptKind::WorkingDirectory, |row| {
                row.child(
                    Button::new("browse-shell-cwd")
                        .label("浏览…")
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.browse_shell_cwd(cx);
                        })),
                )
            });
        v_flex()
            .id("shell-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .items_center()
            .justify_center()
            .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(
                v_flex()
                    .id("shell-form")
                    .w(px(width))
                    .gap_3()
                    .p_5()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .shadow_md()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(rgpui::FontWeight(600.0))
                                    .child(title),
                            )
                            .child(Button::new("close-shell").label("×").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.cancel_shell_prompt(cx);
                                }),
                            )),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child(field_label))
                            .child(input_row),
                    )
                    .child(div().text_xs().text_color(pal.muted).child(hint))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("commit-shell")
                                    .label("保存")
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.commit_shell_prompt(cx);
                                    })),
                            )
                            .child(Button::new("cancel-shell").label("取消").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.cancel_shell_prompt(cx);
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }

    /// 密码输入面板。
    fn render_pending_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(pending) = self.pending.as_ref() else {
            return div().into_any_element();
        };
        let host = pending.host.addr();
        let hint = match pending.kind {
            PromptKind::Password => format!("连接 {host} 需要密码"),
            PromptKind::Passphrase => format!(
                "连接 {host} 的私钥已加密，需要口令（{}）",
                if pending.host.key_inline {
                    "内嵌私钥"
                } else if pending.host.key_path.is_empty() {
                    "未填路径"
                } else {
                    &pending.host.key_path
                }
            ),
        };
        let hint = if self.vault.is_some() {
            hint
        } else {
            format!("{hint}（加密库不可用，仅本次使用，不保存）")
        };
        let save_label = match pending.kind {
            PromptKind::Password => "保存密码（加密存入本地凭据库）",
            PromptKind::Passphrase => "保存口令（加密存入本地凭据库）",
        };
        let save_row = if self.vault.is_some() && pending.host.id != 0 {
            let checked = pending.save_password;
            Some(
                h_flex()
                    .id("save-password")
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(pending) = this.pending.as_mut() {
                            pending.save_password = !pending.save_password;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .text_sm()
                            .text_color(if checked { pal.text } else { pal.muted })
                            .child(if checked { "☑" } else { "☐" }),
                    )
                    .child(div().text_sm().text_color(pal.muted).child(save_label)),
            )
        } else {
            None
        };
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(pal.card_bg)
            .child(div().text_sm().child(hint))
            .child(styled_input(
                Input::new(&pending.password).content_type(InputContentType::Password),
                &pal,
            ))
            .children(save_row)
            .child(
                h_flex()
                    .gap_2()
                    .child(Button::new("connect").label("连接").primary().on_click(
                        cx.listener(|this, _, window, cx| this.confirm_pending(window, cx)),
                    ))
                    .child(
                        Button::new("cancel-connect")
                            .label("取消")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_pending(cx))),
                    ),
            )
            .into_any_element()
    }

    /// 右侧主区：当前终端（会话标签已并入标题栏）。
    fn render_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = self.palette(cx);
        let tab_bar = if self.tabs.is_empty() {
            div().into_any_element()
        } else {
            h_flex()
                .h(px(44.0))
                .flex_shrink_0()
                .bg(pal.tab_bar_bg)
                .border_b_1()
                .border_color(pal.border)
                // 外层裁切、内层横向滚动：标签一多就溢出可见区，滚动而非挤压
                // （与 rgpui 自带的 TabBar 同一套做法）。滚轮在横向滚动容器上
                // 直接映射为水平滚动，不需要另加滚动条。
                .child(
                    h_flex()
                        .id("tabs-strip")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .items_center()
                        .pl_2()
                        .overflow_x_scroll()
                        .restrict_scroll_to_axis()
                        .track_scroll(&self.tab_scroll)
                        .children(self.tabs.iter().map(|tab| {
                            let id = tab.id;
                            let active = self.active_tab == Some(id);
                            let tab_id = SharedString::from(format!("tab-{id}"));
                            let close_id = SharedString::from(format!("tab-close-{id}"));
                            h_flex()
                                .id(tab_id)
                                // 平铺式标签：不描边、标签间不留缝，活动项靠底色 + 顶部强调线区分。
                                .relative()
                                .h_full()
                                .min_w(px(160.0))
                                .flex_shrink_0()
                                .px_3()
                                .gap_2()
                                .items_center()
                                .bg(if active {
                                    pal.tab_active_bg
                                } else {
                                    Hsla::transparent_black()
                                })
                                .text_color(if active {
                                    pal.tab_active_fg
                                } else {
                                    pal.tab_fg
                                })
                                .when(!active, |this| this.hover(|this| this.bg(pal.card_hover)))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.activate_tab(id, window, cx);
                                    cx.notify();
                                }))
                                .when(active, |this| {
                                    this.child(
                                        div()
                                            .absolute()
                                            .top_0()
                                            .left_0()
                                            .right_0()
                                            .h(px(2.0))
                                            .bg(pal.accent),
                                    )
                                })
                                // 结束的会话在后台也要一眼看见：标题前加一个危险色圆点。
                                .when(tab.ended, |this| {
                                    this.child(
                                        div()
                                            .w(px(6.0))
                                            .h(px(6.0))
                                            .flex_shrink_0()
                                            .rounded(px(3.0))
                                            .bg(pal.danger),
                                    )
                                })
                                .child(div().flex_1().min_w_0().truncate().child(tab.title.clone()))
                                // 只有活动标签常驻 ×：非活动项的 × 更挡标题，切换过去再关即可。
                                .when(active, |this| {
                                    this.child(
                                        div()
                                            .id(close_id)
                                            .text_base()
                                            .text_color(pal.muted)
                                            .child("×")
                                            .cursor_pointer()
                                            .hover(|this| this.text_color(pal.danger))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.close_tab(id, cx);
                                            })),
                                    )
                                })
                        })),
                )
                .into_any_element()
        };

        let terminal_bg = self.terminal_bg(cx);
        // 分屏：两个面板并排/上下，可拖中间分隔条；点哪侧就切到哪个标签。
        let split_pair = self.view_split.as_ref().and_then(|view| {
            let first = self.tabs.iter().find(|tab| tab.id == view.first)?;
            let second = self.tabs.iter().find(|tab| tab.id == view.second)?;
            Some((view, first, second))
        });
        let body = if let Some((view, first, second)) = split_pair {
            let first_active = self.active_tab == Some(first.id);
            // SplitPane 给每个面板的容器是 block 布局：面板本身用 flex_1 在 block 父级里
            // 拿不到高度，会塌成只剩内边距。外面再套一层定高的 v_flex 让终端铺满。
            let first_pane = v_flex().size_full().child(self.render_tab_pane(
                first,
                first_active,
                true,
                terminal_bg,
                cx,
            ));
            let second_pane = v_flex().size_full().child(self.render_tab_pane(
                second,
                !first_active,
                true,
                terminal_bg,
                cx,
            ));
            div()
                .flex_1()
                .min_h_0()
                .child(
                    SplitPane::new(view.pane.clone())
                        .direction(view.direction)
                        .first(first_pane)
                        .second(second_pane),
                )
                .into_any_element()
        } else {
            self.active_tab
                .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
                .map(|tab| self.render_tab_pane(tab, true, false, terminal_bg, cx))
                .unwrap_or_else(|| {
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_base()
                                .text_color(pal.muted)
                                .child("从左侧点击主机账号开始连接"),
                        )
                        .into_any_element()
                })
        };

        v_flex().flex_1().h_full().child(tab_bar).child(body)
    }

    /// 主区里的一个会话面板：终端内容 + 内边距 + 断开横幅。
    ///
    /// `highlight` 只在分屏时给活动一侧加一圈强调色边框——单屏不需要。
    fn render_tab_pane(
        &self,
        tab: &SessionTab,
        active: bool,
        highlight: bool,
        terminal_bg: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let id = tab.id;
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(terminal_bg)
            .px_3()
            .py_2()
            .when(highlight && active, |this| {
                this.border_1().border_color(pal.accent)
            })
            // 分屏时点一下另一侧即把焦点/状态栏切过去；点击落在终端上也会冒泡到这里。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if this.active_tab != Some(id) {
                        this.activate_tab(id, window, cx);
                        cx.notify();
                    }
                }),
            )
            .child(tab.terminal_view.clone())
            .when(tab.ended, |this| {
                this.child(self.render_ended_banner(tab, cx))
            })
            .into_any_element()
    }

    /// 会话断开横幅：浮在终端顶部，说明连接已结束并提供重连入口。
    ///
    /// 标签不自动关闭（与 WindTerm 一致）：终端内容留在原处可回看，按回车
    /// 或点「重新连接」在原标签内重建会话；开启了自动重连时先按退避等待并显示
    /// 倒计时，可随时取消。凭据已被服务器拒绝时不提供重连（重试同一份秘密只会
    /// 再失败一次），提示回到列表重新点击。
    fn render_ended_banner(&self, tab: &SessionTab, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let theme = cx.theme();
        let fg = theme.warning_foreground;
        let id = tab.id;
        let reconnectable = tab.resume.is_some();
        let attempt = tab.auto_retry_attempt;
        let pending = tab.auto_retry_pending && reconnectable;
        let exhausted = reconnectable && !pending && attempt >= AUTO_RECONNECT_MAX;
        let hint = if pending {
            format!(
                "正在自动重连（第 {attempt} 次，{} 秒后）",
                auto_reconnect_delay(attempt).as_secs()
            )
        } else if exhausted {
            "自动重连已停止，按回车手动重连".to_string()
        } else if reconnectable {
            "按回车重新连接".to_string()
        } else {
            "请从左侧重新点击该主机连接".to_string()
        };
        h_flex()
            .id(SharedString::from(format!("ended-banner-{id}")))
            .absolute()
            .top(px(8.0))
            .left(px(8.0))
            .right(px(8.0))
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .rounded(px(6.0))
            .bg(theme.warning)
            .text_color(fg)
            .shadow_md()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight(600.0))
                    .child("远程通道已关闭"),
            )
            .child(div().text_sm().child(if tab.failed {
                "连接中断。"
            } else {
                "会话已断开连接。"
            }))
            .child(div().flex_1())
            .child(div().text_xs().child(hint))
            .when(pending, |this| {
                this.child(
                    Button::new(SharedString::from(format!("cancel-retry-{id}")))
                        .label("取消自动重连")
                        .secondary()
                        .outline()
                        .compact()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.cancel_auto_reconnect(id, cx);
                        })),
                )
            })
            .when(reconnectable, |this| {
                this.child(
                    Button::new(SharedString::from(format!("reconnect-{id}")))
                        .label("重新连接")
                        .primary()
                        .compact()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.reconnect_tab(id, window, cx);
                        })),
                )
            })
            .child(
                div()
                    .id(SharedString::from(format!("ended-close-{id}")))
                    .px_1()
                    .text_base()
                    .cursor_pointer()
                    .text_color(fg)
                    .hover(|this| this.text_color(pal.danger))
                    .child("×")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_tab(id, cx);
                    })),
            )
            .into_any_element()
    }

    /// 底部状态栏：左侧瞬态状态 + 活动会话信息（按用户勾选的显示项拼装），右侧会话计数。
    fn render_status_bar(&self) -> StatusBar {
        let active = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id));

        let mut left = vec![StatusBarItem::new(self.status.clone()).id("status")];
        if let Some(tab) = active {
            if self.status_bar.addr {
                left.push(StatusBarItem::new(tab.addr.clone()).id("addr"));
            }
            if let (true, Some(auth)) = (self.status_bar.auth, tab.auth) {
                left.push(StatusBarItem::new(auth_label(auth)).id("auth").muted(true));
            }
            // 远程基本信息由 SSH 建立后的 exec 探测任务异步写入 tab.remote_info。
            if let Some(info) = tab.remote_info.as_ref() {
                for metric in Metric::ALL {
                    if !self.status_bar.metric_on(metric) {
                        continue;
                    }
                    // 远端没探到该项就整条不显示，不留空占位。
                    if let Some(text) = info.item(metric) {
                        left.push(StatusBarItem::new(SharedString::from(text)).id(metric.id()));
                    }
                }
            }
        }
        let right = vec![
            StatusBarItem::new(format!("会话 {}", self.tabs.len()))
                .id("sessions")
                .muted(true),
        ];
        StatusBar::new().left(left).right(right)
    }

    /// 勾选状态栏显示项：即时生效并持久化（库内优先，降级时写 settings.json）。
    fn set_status_pref(&mut self, pref: StatusPref, on: bool, cx: &mut Context<Self>) {
        match pref {
            StatusPref::Addr => self.status_bar.addr = on,
            StatusPref::Auth => self.status_bar.auth = on,
            StatusPref::Metric(metric) => self.status_bar.set_metric(metric, on),
        }
        cx.notify();
        self.persist_status_bar(cx);
    }

    /// 状态栏偏好落库；加密库不可用时退回旧 settings.json。
    fn persist_status_bar(&self, cx: &mut Context<Self>) {
        let json = self.status_bar.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_status_bar(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("status_bar", &json).await {
                log::warn!("写入库内状态栏偏好失败: {error}");
            }
        })
        .detach();
    }

    /// 终端底色：内边距要与终端内容同色，否则在浅色主题下会显成一圈白框。
    fn terminal_bg(&self, cx: &App) -> Hsla {
        let Some(manager) = cx.try_global::<ThemeManager>() else {
            return self.palette(cx).window_bg;
        };
        manager
            .get_theme(&self.terminal.theme)
            .or_else(|| Some(manager.current_theme()))
            .map(|theme| theme.background)
            .unwrap_or_else(|| self.palette(cx).window_bg)
    }

    /// 由偏好组装终端渲染样式。配色取自 rgpui-term 的 `ThemeManager` 预设
    /// （方案名读不到时回落当前主题，避免解析不到颜色）；字体族沿用默认等宽字体。
    fn terminal_text_style(&self, cx: &App) -> TextStyle {
        let mut style = TextStyle::default();
        style.font_size = AbsoluteLength::Pixels(px(self.terminal.font_size));
        style.line_height_multiplier = self.terminal.line_height;
        let theme = cx
            .try_global::<ThemeManager>()
            .and_then(|manager| manager.get_theme(&self.terminal.theme).cloned())
            .or_else(|| {
                cx.try_global::<ThemeManager>()
                    .map(|manager| manager.current_theme().clone())
            });
        if let Some(theme) = theme {
            style.foreground = theme.foreground;
            style.background = theme.background;
            style.theme = theme;
        }
        style
    }

    /// 把当前样式刷到所有已开终端：设置里改一项即刻见效，无需重连。
    fn apply_terminal_style(&self, style: &TextStyle, cx: &mut App) {
        for tab in &self.tabs {
            let style = style.clone();
            tab.terminal_view.update(cx, |view, cx| {
                *view.text_style_mut() = style;
                cx.notify();
            });
        }
    }

    /// 终端外观改动：记账 → 刷新所有终端 → 持久化（库内优先，降级时写 settings.json）。
    fn set_terminal_prefs(&mut self, mut prefs: TerminalPrefs, cx: &mut Context<Self>) {
        prefs.clamp();
        if prefs == self.terminal {
            return;
        }
        self.terminal = prefs;
        // 自定义色改了就重新注册，之后 `terminal_text_style` 才取得到新配色。
        Self::register_custom_theme(&self.terminal.custom, cx);
        let style = self.terminal_text_style(cx);
        self.apply_terminal_style(&style, cx);
        cx.notify();
        self.persist_terminal(cx);
    }

    /// 字号增减一档（`steps` 为正负档数）。
    fn nudge_font_size(&mut self, steps: i32, cx: &mut Context<Self>) {
        let mut prefs = self.terminal.clone();
        prefs.nudge_font_size(steps);
        self.set_terminal_prefs(prefs, cx);
    }

    /// 行高增减一档。
    fn nudge_line_height(&mut self, steps: i32, cx: &mut Context<Self>) {
        let mut prefs = self.terminal.clone();
        prefs.nudge_line_height(steps);
        self.set_terminal_prefs(prefs, cx);
    }

    /// 切换终端配色方案。
    fn set_terminal_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let mut prefs = self.terminal.clone();
        prefs.theme = name.to_string();
        self.set_terminal_prefs(prefs, cx);
    }

    /// 把「自定义」配色注册进全局 `ThemeManager`（重复调用即覆盖）。
    fn register_custom_theme(colors: &TerminalColors, cx: &mut App) {
        if !cx.has_global::<ThemeManager>() {
            return;
        }
        ThemeManager::global_mut(cx).add_theme(ThemeDefinition {
            name: TERMINAL_THEME_CUSTOM.to_string(),
            theme: colors.to_config().to_theme(),
        });
    }

    /// 某个自定义色输入框内容变化：合法十六进制才提交，非法输入保留上一个有效色。
    fn commit_custom_color(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(input) = self.custom_color_inputs.get(index) else {
            return;
        };
        let text = input.read(cx).value().to_string();
        let Some(normalized) = normalize_hex(&text) else {
            return;
        };
        if self.terminal.custom.hex_field(index) == normalized {
            return;
        }
        let mut prefs = self.terminal.clone();
        prefs.custom.set_hex_field(index, normalized);
        self.set_terminal_prefs(prefs, cx);
    }

    /// 恢复自定义配色的默认值（One Dark），并同步输入框显示。
    fn reset_custom_colors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut prefs = self.terminal.clone();
        prefs.custom = TerminalColors::default();
        self.set_terminal_prefs(prefs, cx);
        self.sync_custom_color_inputs(window, cx);
    }

    /// 把偏好里的颜色回填到输入框（打开设置弹窗时调用，库内值可能晚于启动到达）。
    fn sync_custom_color_inputs(&self, window: &mut Window, cx: &mut App) {
        for (index, input) in self.custom_color_inputs.iter().enumerate() {
            let value = self.terminal.custom.hex_field(index);
            input.update(cx, |state, cx| {
                if state.value().as_ref() != value {
                    state.set_value(value, window, cx);
                }
            });
        }
    }

    /// 可选的终端配色方案名（`ThemeManager` 预设，已按名排序）。
    fn terminal_themes(cx: &App) -> Vec<SharedString> {
        match cx.try_global::<ThemeManager>() {
            Some(manager) => manager
                .available_themes()
                .into_iter()
                .map(SharedString::from)
                .collect(),
            None => vec![TERMINAL_THEME_DEFAULT.into()],
        }
    }

    /// 终端偏好落库；加密库不可用时退回旧 settings.json。
    fn persist_terminal(&self, cx: &mut Context<Self>) {
        let json = self.terminal.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_terminal(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("terminal", &json).await {
                log::warn!("写入库内终端偏好失败: {error}");
            }
        })
        .detach();
    }

    /// 会话行为改动：记账 → 持久化（库内优先，降级时写 settings.json）。
    fn set_session_prefs(&mut self, mut prefs: SessionPrefs, cx: &mut Context<Self>) {
        prefs.clamp();
        if prefs == self.session_prefs {
            return;
        }
        self.session_prefs = prefs;
        cx.notify();
        self.persist_session(cx);
    }

    /// 开关断线自动重连。
    fn set_auto_reconnect(&mut self, auto_reconnect: bool, cx: &mut Context<Self>) {
        let mut prefs = self.session_prefs.clone();
        prefs.auto_reconnect = auto_reconnect;
        self.set_session_prefs(prefs, cx);
    }

    /// 连接超时增减一档。
    fn nudge_connect_timeout(&mut self, steps: i32, cx: &mut Context<Self>) {
        let mut prefs = self.session_prefs.clone();
        prefs.nudge_connect_timeout(steps);
        self.set_session_prefs(prefs, cx);
    }

    /// 会话行为偏好落库；加密库不可用时退回旧 settings.json。
    fn persist_session(&self, cx: &mut Context<Self>) {
        let json = self.session_prefs.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_session(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("session", &json).await {
                log::warn!("写入库内会话偏好失败: {error}");
            }
        })
        .detach();
    }

    /// 分组偏好落库；加密库不可用时退回旧 settings.json。
    /// 把跳板机映射回填到主机列表。
    ///
    /// 只对加密库模式生效：降级模式下 `proxy_jump` 直接序列化在 sessions.json 里，
    /// 这里再按（可能全为 0 的）id 覆盖反而会把它抹掉。
    fn apply_jumps(&mut self) {
        if self.vault.is_none() {
            return;
        }
        for host in &mut self.hosts {
            host.proxy_jump = self.jumps.jump_of(host.id);
        }
    }

    /// 记录主机与跳板机的绑定关系。
    ///
    /// 加密库模式写 `settings` 的 `jumps` 键；降级模式跳板机字段直接随
    /// sessions.json 落盘，这里不动（否则会按 id=0 覆盖成空）。
    fn note_jump(&mut self, host: &HostConfig, cx: &mut Context<Self>) {
        if self.vault.is_none() || self.jumps.jump_of(host.id) == host.proxy_jump {
            return;
        }
        self.jumps.set(host.id, host.proxy_jump);
        self.persist_jumps(cx);
    }

    /// 跳板机映射落库；加密库不可用时退回旧 settings.json。
    fn persist_jumps(&self, cx: &mut Context<Self>) {
        let json = self.jumps.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_jumps(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("jumps", &json).await {
                log::warn!("写入库内跳板机映射失败: {error}");
            }
        })
        .detach();
    }

    fn persist_sidebar(&self, cx: &mut Context<Self>) {
        let json = self.sidebar.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_sidebar(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("sidebar", &json).await {
                log::warn!("写入库内分组偏好失败: {error}");
            }
        })
        .detach();
    }

    /// 连接成功（shell 已开）后记一笔最近连接：去重后置顶，超出上限截断。
    /// 降级模式（无加密库）主机 id 不可靠，只保留内存值，不落盘。
    fn record_recent_host(&mut self, host_id: i64, cx: &mut Context<Self>) {
        if host_id == 0 || !self.hosts.iter().any(|host| host.id == host_id) {
            return;
        }
        self.recent_hosts.retain(|id| *id != host_id);
        self.recent_hosts.insert(0, host_id);
        self.recent_hosts.truncate(RECENT_HOSTS_MAX);
        self.persist_recent_hosts(cx);
    }

    fn persist_recent_hosts(&self, cx: &mut Context<Self>) {
        let Some(vault) = self.vault.clone() else {
            return;
        };
        let ids = self.recent_hosts.clone();
        cx.spawn(async move |_, _| {
            let Ok(json) = serde_json::to_string(&ids) else {
                return;
            };
            if let Err(error) = vault.set_setting("recent", &json).await {
                log::warn!("写入库内最近连接失败: {error}");
            }
        })
        .detach();
    }

    /// 端口转发规则落库；加密库不可用时退回旧 settings.json。
    fn persist_forwards(&self, cx: &mut Context<Self>) {        let json = self.forward_prefs.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_forwards(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("forwards", &json).await {
                log::warn!("写入库内转发规则失败: {error}");
            }
        })
        .detach();
    }
}

/// 端口转发列表的开关：轨道 + 滑块，点击整块切换。
fn forward_switch(
    id: String,
    on: bool,
    pal: &Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(SharedString::from(id))
        .w(px(34.0))
        .h(px(18.0))
        .p(px(2.0))
        .flex()
        .items_center()
        .when(on, |el| el.justify_end())
        .when(!on, |el| el.justify_start())
        .rounded(px(9.0))
        .bg(if on { pal.accent } else { pal.segment_bg })
        .border_1()
        .border_color(if on { pal.accent } else { pal.border })
        .cursor_pointer()
        .on_click(on_click)
        .child(
            div()
                .size(px(14.0))
                .rounded(px(7.0))
                .bg(if on { pal.card_bg } else { pal.muted }),
        )
}

/// 设置分页里的 −/＋ 步进按钮：中性描边小按钮，与「关闭」等按钮保持同一观感。
fn stepper(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + Send + Sync + 'static,
) -> Button {
    Button::new(id)
        .label(label)
        .secondary()
        .outline()
        .compact()
        .on_click(on_click)
}

/// 带标题的输入框行。
fn labeled_field(
    label: &'static str,
    input: &Entity<InputState>,
    pal: &Palette,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().text_sm().child(label))
        .child(styled_input(Input::new(input), pal))
}

/// 统一输入框外观：随主题取底色/文字/边框，保证未聚焦时文字与边框都清晰可辨。
fn styled_input(input: Input, pal: &Palette) -> Input {
    input
        .bg(pal.input_bg)
        .text_color(pal.input_text)
        .border_1()
        .border_color(pal.input_border)
}

/// 下载保存对话框的起始目录：优先系统下载目录，缺失则退回当前目录。
fn downloads_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    home.map(|home| home.join("Downloads"))
        .filter(|dir| dir.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Focusable for RuLinkApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn main() {
    rgpui::init_logging();

    // 带目标参数 → 一次性快速连接兜底；无参数 → 纯 GUI 会话管理器。
    let quick = if std::env::args().nth(1).is_some() {
        match SshTarget::from_args() {
            Ok(target) => Some(target),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    let app = rgpui_platform::application();
    app.run(move |cx| {
        rgpui::tokio::init(cx);
        // 主题 + 输入子系统必须初始化：否则输入框无配色且
        // 退格/删除等键位动作未注册（无法编辑已输入内容）。
        rgpui::theme::init(cx);
        // theme::init 默认切到亮色；库内主题要等开库后才能读到，这里先按
        // 旧 settings.json（降级/迁移前）或默认暗色起窗，避免闪一下亮色。
        let dark = SessionStore::new(&StorageMode::detect())
            .load_settings()
            .dark();
        rgpui::theme::Theme::change(
            if dark {
                rgpui::theme::ThemeMode::Dark
            } else {
                rgpui::theme::ThemeMode::Light
            },
            None,
            cx,
        );
        rgpui::input_ui::init(cx);
        // 全局快捷键：与菜单项上的 action 同名，菜单里会自动显示这组按键。
        cx.bind_keys([
            KeyBinding::new("ctrl-b", ToggleSidebar, None),
            KeyBinding::new("ctrl-t", ToggleTheme, None),
            KeyBinding::new("ctrl-n", NewHost, None),
            KeyBinding::new("ctrl-shift-t", NewLocalTerminal, None),
            // 标签多到溢出后光靠鼠标滚动不够快，配上循环切换。
            KeyBinding::new("ctrl-tab", NextTab, None),
            KeyBinding::new("ctrl-shift-tab", PrevTab, None),
            KeyBinding::new("ctrl-pagedown", NextTab, None),
            KeyBinding::new("ctrl-pageup", PrevTab, None),
            KeyBinding::new("ctrl-,", OpenSettings, None),
            // 文件传输：给当前活动会话开 SFTP 上传 / 下载弹窗。
            KeyBinding::new("ctrl-shift-u", OpenTransfer, None),
            // 端口转发：给当前活动会话配置本地监听 → 远端目标。
            KeyBinding::new("ctrl-shift-f", OpenForward, None),
            // 分屏：左右 / 上下；同方向再按一次收起，换方向原地换排布。
            KeyBinding::new("ctrl-shift-d", SplitRight, None),
            KeyBinding::new("ctrl-shift-e", SplitDown, None),
            // Tab 在 rgpui 的 Root 上下文里被绑成焦点切换，而按键绑定分发早于 on_key_down，
            // 终端元素因此永远收不到 \x09（shell 补全失效）。在 Terminal 上下文用 NoAction
            // 抢占：它比 Root 更深，分发解析到它就停止，按键原样落到 on_key_down 转发给 PTY。
            KeyBinding::new("tab", NoAction {}, Some("Terminal")),
            KeyBinding::new("shift-tab", NoAction {}, Some("Terminal")),
        ]);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.0), px(720.0)),
                    cx,
                ))),
                titlebar: Some(rgpui::TitlebarOptions {
                    title: Some("RuLink".into()),
                    // 隐藏系统标题栏，改由应用自绘（品牌 + 菜单 + 窗口控制按钮）。
                    appears_transparent: true,
                    ..Default::default()
                }),
                // 客户区装饰：窗口样式改为 WS_THICKFRAME 无边框（DWM 仍画圆角与可拖边框），
                // 拖拽与窗口控制由标题栏里显式登记的 WindowControlArea 命中区交给系统。
                app_owns_titlebar_drag: true,
                ..Default::default()
            },
            |window, cx| {
                let quick = quick.clone();
                let app = cx.new(|cx| RuLinkApp::new(quick, window, cx));
                // 关闭按钮改为隐藏到托盘：窗口不销毁，会话后台保活；
                // 返回 false 中断关闭，真正退出走托盘"退出"（cx.quit 直接投 WM_QUIT，不经此回调）。
                window.on_window_should_close(cx, |window, _cx| {
                    window.hide_window();
                    false
                });
                app
            },
        );
        let window_handle = match opened {
            Ok(handle) => handle,
            Err(_) => {
                cx.quit();
                return;
            }
        };
        cx.activate(true);

        // 系统托盘：品牌图标 + 悬停提示 + 右键菜单（活动会话 / 显示主界面 / 退出）。
        // 图标以 32px PNG 内嵌，Windows 走 image 解码路径最稳；不依赖运行时文件路径。
        const TRAY_ICON: &[u8] = include_bytes!("../assets/icons/tray-32.png");
        cx.set_tray_icon(Some(TRAY_ICON));
        // 菜单与提示由应用按会话列表同步（隐藏到托盘后也能直接跳回某个标签）。
        let _ = window_handle.update(cx, |app, _, cx| app.sync_tray(cx));
        cx.on_tray_menu_action(move |id: SharedString, cx: &mut App| {
            if let Some(rest) = id.strip_prefix("tab:") {
                if let Ok(tab_id) = rest.parse::<usize>() {
                    let _ = window_handle.update(cx, |app, window, cx| {
                        window.activate_window();
                        app.activate_tab(tab_id, window, cx);
                    });
                }
                return;
            }
            match id.as_ref() {
                "quit" => cx.quit(),
                "show" => {
                    // 从托盘恢复：把隐藏的主窗口重新带到前台。
                    window_handle
                        .update(cx, |_, window, _| window.activate_window())
                        .ok();
                }
                _ => {}
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{
        AUTO_RECONNECT_MAX, CONNECT_TIMEOUT_DEFAULT, CONNECT_TIMEOUT_MAX, CONNECT_TIMEOUT_MIN,
        CUSTOM_ANSI_BASE, CUSTOM_BRIGHT_BASE, CUSTOM_DEFAULT_ANSI, CUSTOM_DEFAULT_BRIGHT,
        FONT_SIZE_MAX, FONT_SIZE_MIN, ForwardPrefs, JumpPrefs, LINE_HEIGHT_MAX, LINE_HEIGHT_MIN,
        LocalShellPrefs, Metric, SavedForward, SessionPrefs, SidebarPrefs, StatusBarPrefs,
        TERMINAL_THEME_DEFAULT, TerminalColors, TerminalPrefs, auto_reconnect_delay, hex_to_hsla,
        normalize_hex,
    };
    use crate::session::ForwardKind;

    fn saved(name: &str, kind: ForwardKind, listen: u16) -> SavedForward {
        SavedForward {
            name: name.to_string(),
            kind,
            listen_port: listen,
            target_host: "127.0.0.1".to_string(),
            target_port: 80,
            auto_start: false,
        }
    }

    #[test]
    fn saved_forward_defaults_and_title() {
        // 旧数据可能只有方向与端口，其余字段回落到默认值。
        let parsed: SavedForward = serde_json::from_str(r#"{"kind":"remote","listen_port":9000}"#)
            .expect("应能解析缺省字段");
        assert_eq!(parsed.target_host, "127.0.0.1");
        assert_eq!(parsed.target_port, 80);
        assert!(!parsed.auto_start);
        assert_eq!(parsed.rule().kind, ForwardKind::Remote);
        assert_eq!(parsed.title(), "R 服务器:9000 → 本机 127.0.0.1:80");

        let named = SavedForward {
            name: "内网 Web".to_string(),
            ..parsed
        };
        assert_eq!(named.title(), "内网 Web");
    }

    #[test]
    fn forward_prefs_insert_replace_and_remove() {
        let mut prefs = ForwardPrefs::default();
        prefs.insert("tester@127.0.0.1:22", saved("", ForwardKind::Local, 8080));
        prefs.insert("tester@127.0.0.1:22", saved("", ForwardKind::Remote, 8080));
        // 同方向同端口视为同一条：原位替换而不是追加。
        prefs.insert("tester@127.0.0.1:22", saved("web", ForwardKind::Local, 8080));
        let list = prefs.rules_for("tester@127.0.0.1:22");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "web");

        // 表单名字留空再保存同一条规则时沿用旧备注，不被清空。
        prefs.insert("tester@127.0.0.1:22", saved("", ForwardKind::Local, 8080));
        assert_eq!(prefs.rules_for("tester@127.0.0.1:22")[0].name, "web");

        // 不同连接互不影响。
        assert!(prefs.rules_for("other@host:22").is_empty());

        let roundtrip = ForwardPrefs::from_json(&prefs.to_json());
        assert_eq!(roundtrip, prefs);

        let local = saved("", ForwardKind::Local, 8080).rule();
        prefs.remove("tester@127.0.0.1:22", &local.id());
        assert_eq!(prefs.rules_for("tester@127.0.0.1:22").len(), 1);
        let remote = saved("", ForwardKind::Remote, 8080).rule();
        prefs.remove("tester@127.0.0.1:22", &remote.id());
        // 最后一条删除后不留空连接键。
        assert!(prefs.rules_for("tester@127.0.0.1:22").is_empty());
        assert_eq!(ForwardPrefs::from_json("{}"), ForwardPrefs::default());
        assert_eq!(ForwardPrefs::from_json("{ not json"), ForwardPrefs::default());
    }

    #[test]
    fn jump_prefs_roundtrip_and_forget() {
        let mut prefs = JumpPrefs::default();
        prefs.set(5, Some(2));
        prefs.set(6, Some(2));
        prefs.set(7, Some(3));
        assert_eq!(prefs.jump_of(5), Some(2));
        assert_eq!(JumpPrefs::from_json(&prefs.to_json()), prefs);

        // 删掉 2：它作为跳板机的引用断掉，别的绑定不受影响。
        let orphaned = prefs.forget(2);
        assert_eq!(orphaned.len(), 2);
        assert!(orphaned.contains(&5) && orphaned.contains(&6));
        assert_eq!(prefs.jump_of(5), None);
        assert_eq!(prefs.jump_of(7), Some(3));

        // 改成直连 = 删条目；空串 / 坏 JSON 回落默认值。
        prefs.set(7, None);
        assert_eq!(prefs.jump_of(7), None);
        assert_eq!(JumpPrefs::from_json(""), JumpPrefs::default());
        assert_eq!(JumpPrefs::from_json("{ not json"), JumpPrefs::default());
    }

    #[test]
    fn local_shell_prefs_name_and_cwd_roundtrip() {
        let mut prefs = LocalShellPrefs::default();
        prefs.rename("pwsh", "主终端");
        prefs.set_cwd("pwsh", r"D:\code");
        prefs.set_args("pwsh", "-NoLogo -NoProfile");
        prefs.set_env("pwsh", "FOO=1\nBAR=two words");
        assert_eq!(prefs.name_of("pwsh"), Some("主终端"));
        assert_eq!(prefs.cwd_of("pwsh"), Some(r"D:\code"));
        assert_eq!(prefs.args_of("pwsh"), ["-NoLogo", "-NoProfile"]);
        assert_eq!(
            prefs.env_map_of("pwsh").get("BAR").map(String::as_str),
            Some("two words")
        );
        assert_eq!(LocalShellPrefs::from_json(&prefs.to_json()), prefs);

        // 空串/空白 = 恢复默认（删条目，不留空壳）。
        prefs.rename("pwsh", "  ");
        prefs.set_cwd("pwsh", " ");
        prefs.set_args("pwsh", " ");
        prefs.set_env("pwsh", "");
        assert_eq!(prefs.name_of("pwsh"), None);
        assert_eq!(prefs.cwd_of("pwsh"), None);
        assert!(prefs.args_of("pwsh").is_empty());
        assert!(prefs.env_map_of("pwsh").is_empty());

        // 旧版 JSON 只有 names：cwd / args / env 缺省不影响读取。
        let legacy = LocalShellPrefs::from_json(r#"{"names":{"cmd":"CMD"}}"#);
        assert_eq!(legacy.name_of("cmd"), Some("CMD"));
        assert_eq!(legacy.cwd_of("cmd"), None);
        assert!(legacy.args_of("cmd").is_empty());
        assert!(legacy.env_of("cmd").is_empty());
    }

    #[test]
    fn status_bar_prefs_default_to_everything_shown() {
        let prefs = StatusBarPrefs::default();
        assert!(prefs.addr && prefs.auth);
        assert!(Metric::ALL.iter().all(|m| prefs.metric_on(*m)));
    }

    #[test]
    fn status_bar_prefs_roundtrip_and_tolerate_partial_json() {
        let mut prefs = StatusBarPrefs::default();
        prefs.addr = false;
        prefs.set_metric(Metric::Mem, false);
        let restored = StatusBarPrefs::from_json(&prefs.to_json());
        assert_eq!(restored, prefs);

        // 只写了一部分指标：未记录的（含以后新增的）一律按"显示"处理。
        let partial =
            StatusBarPrefs::from_json(r#"{"addr":true,"auth":false,"metrics":{"mem":false}}"#);
        assert!(!partial.auth);
        assert!(!partial.metric_on(Metric::Mem));
        assert!(partial.metric_on(Metric::Disk));

        // 空串与损坏内容回落到默认值，不让状态栏被一条坏数据清空。
        assert_eq!(StatusBarPrefs::from_json(""), StatusBarPrefs::default());
        assert_eq!(
            StatusBarPrefs::from_json("{ not json"),
            StatusBarPrefs::default()
        );
    }

    #[test]
    fn sidebar_prefs_group_lifecycle_keeps_hosts_alive() {
        let mut prefs = SidebarPrefs::default();
        assert!(prefs.add("生产"));
        assert!(prefs.add(" 家 "), "名称应自动去空白");
        assert!(!prefs.add("生产"), "同名分组应被拒绝");
        assert!(!prefs.add("未分组"), "未分组是保留名");
        assert!(!prefs.add("   "), "空名应被拒绝");
        assert_eq!(prefs.names().collect::<Vec<_>>(), vec!["生产", "家"]);

        prefs.assign_to(7, Some("生产"));
        prefs.assign_to(9, Some("生产"));
        prefs.assign_to(11, Some("家"));
        assert_eq!(prefs.group_of(7), Some("生产"));

        // 改名要连带改写组内主机的归属，否则主机会静默掉回未分组。
        assert!(prefs.rename("生产", "预发"));
        assert_eq!(prefs.group_of(7), Some("预发"));
        assert_eq!(prefs.group_of(9), Some("预发"));
        assert!(prefs.group_of(11).is_some());
        assert!(!prefs.rename("不存在", "X"));
        assert!(!prefs.rename("预发", "家"), "改名不能撞已有分组");

        // 删除分组只退回归属，主机本身保留。
        prefs.remove("预发");
        assert_eq!(prefs.group_of(7), None);
        assert_eq!(prefs.group_of(11), Some("家"));

        // 归属指向一个已不存在的组：group_of 视为未分组。
        let mut orphan = SidebarPrefs::default();
        orphan.assign_to(3, Some("被删掉的组"));
        assert_eq!(orphan.group_of(3), None);

        // 主机删除后清理残留归属，避免 JSON 越长越脏。
        prefs.forget(11);
        assert!(prefs.assign.is_empty());
    }

    #[test]
    fn sidebar_prefs_manual_order_moves_and_forgets() {
        let mut prefs = SidebarPrefs::default();
        prefs.sync_order(&[1, 2, 3]);
        assert_eq!(prefs.rank_of(1), 0);
        assert_eq!(prefs.rank_of(2), 1);

        // 上移 / 移到最前都走 move_before（把本机插到锚点前）。
        prefs.move_before(3, 1);
        assert_eq!(prefs.order, vec![3, 1, 2]);
        // 下移＝把下一个邻居挪到本机前。
        prefs.move_before(2, 1);
        assert_eq!(prefs.order, vec![3, 2, 1]);

        // 新主机追加到尾部；未登记 id 的 rank 视为最后。
        prefs.sync_order(&[1, 2, 3, 9]);
        assert_eq!(prefs.rank_of(9), 3);
        assert_eq!(prefs.rank_of(42), usize::MAX);
        // 已删除的主机在 sync 时清出去。
        prefs.sync_order(&[1, 3, 9]);
        assert_eq!(prefs.order, vec![3, 1, 9]);

        // 删除主机同时清排序位，不留孤儿 id。
        prefs.forget(2);
        assert_eq!(prefs.order, vec![3, 1, 9]);
        assert_eq!(SidebarPrefs::from_json(&prefs.to_json()), prefs);
    }

    #[test]
    fn sidebar_prefs_collapse_is_per_group_and_survives_json() {
        let mut prefs = SidebarPrefs::default();
        prefs.add("生产");
        assert!(!prefs.collapsed("生产"));
        prefs.toggle("生产");
        assert!(prefs.collapsed("生产"));
        prefs.toggle("生产");
        assert!(!prefs.collapsed("生产"));

        // 未分组没有 GroupEntry，折叠态记在独立字段上。
        prefs.toggle("未分组");
        assert!(prefs.collapsed("未分组"));

        let restored = SidebarPrefs::from_json(&prefs.to_json());
        assert_eq!(restored, prefs);

        assert_eq!(SidebarPrefs::from_json(""), SidebarPrefs::default());
        assert_eq!(
            SidebarPrefs::from_json("{ not json"),
            SidebarPrefs::default()
        );
    }

    #[test]
    fn sidebar_prefs_expand_clears_collapse_for_group_and_ungrouped() {
        // 新主机落进折叠组时要把组展开，否则保存后像"没存进去"。
        let mut prefs = SidebarPrefs::default();
        prefs.add("生产");
        prefs.toggle("生产");
        prefs.toggle("未分组");
        prefs.expand("生产");
        prefs.expand("未分组");
        assert!(!prefs.collapsed("生产"));
        assert!(!prefs.collapsed("未分组"));
        // 不存在的组名不应改动任何状态。
        prefs.expand("没有这个组");
        assert!(!prefs.collapsed("生产"));
    }

    #[test]
    fn session_prefs_roundtrip_and_clamp() {
        let prefs = SessionPrefs::default();
        assert!(prefs.auto_reconnect, "默认开启自动重连");
        assert_eq!(prefs.connect_timeout, CONNECT_TIMEOUT_DEFAULT);
        assert_eq!(SessionPrefs::from_json(&prefs.to_json()), prefs);

        // 越界/坏数据夹回区间，空串与损坏 JSON 回默认值。
        let tiny = SessionPrefs::from_json(r#"{"auto_reconnect":false,"connect_timeout":1}"#);
        assert!(!tiny.auto_reconnect);
        assert_eq!(tiny.connect_timeout, CONNECT_TIMEOUT_MIN);
        let huge = SessionPrefs::from_json(r#"{"connect_timeout":9999}"#);
        assert_eq!(huge.connect_timeout, CONNECT_TIMEOUT_MAX);
        assert_eq!(SessionPrefs::from_json(""), SessionPrefs::default());
        assert_eq!(
            SessionPrefs::from_json("{ not json"),
            SessionPrefs::default()
        );

        // 步进在区间端点停住。
        let mut low = SessionPrefs {
            connect_timeout: CONNECT_TIMEOUT_MIN,
            ..Default::default()
        };
        low.nudge_connect_timeout(-3);
        assert_eq!(low.connect_timeout, CONNECT_TIMEOUT_MIN);
        let mut high = SessionPrefs {
            connect_timeout: CONNECT_TIMEOUT_MAX,
            ..Default::default()
        };
        high.nudge_connect_timeout(3);
        assert_eq!(high.connect_timeout, CONNECT_TIMEOUT_MAX);
    }

    #[test]
    fn auto_reconnect_backoff_grows_and_caps() {
        let delays: Vec<u64> = (1..=AUTO_RECONNECT_MAX)
            .map(|attempt| auto_reconnect_delay(attempt).as_secs())
            .collect();
        assert_eq!(delays, vec![1, 2, 5, 10, 20]);
        // 超出上限的序号也封顶在 20 秒，不会算出离谱的等待。
        assert_eq!(auto_reconnect_delay(99).as_secs(), 20);
    }

    #[test]
    fn terminal_prefs_roundtrip_keeps_all_three_fields() {
        let mut prefs = TerminalPrefs::default();
        prefs.nudge_font_size(3);
        prefs.nudge_line_height(2);
        prefs.theme = "Dracula".into();
        prefs
            .custom
            .set_hex_field(CUSTOM_BRIGHT_BASE + 3, "#123456".into());
        let restored = TerminalPrefs::from_json(&prefs.to_json());
        assert_eq!(restored.font_size, prefs.font_size);
        assert!((restored.line_height - prefs.line_height).abs() < 1e-3);
        assert_eq!(restored.theme, "Dracula");
        assert_eq!(restored.custom, prefs.custom);
        // 旧版 JSON 没有 custom：按 One Dark 初始值补齐，不影响其余字段。
        let legacy = TerminalPrefs::from_json(r#"{"font_size":16.0}"#);
        assert_eq!(legacy.custom, TerminalColors::default());
    }

    #[test]
    fn custom_colors_normalize_and_repair_bad_values() {
        // 大小写、省略 #、三位缩写都规范化成 #RRGGBB。
        assert_eq!(normalize_hex(" #abc "), Some("#AABBCC".to_string()));
        assert_eq!(normalize_hex("a1b2c3"), Some("#A1B2C3".to_string()));
        // 长度不对或含非十六进制字符一律拒绝，输入框保留上一个有效色。
        assert_eq!(normalize_hex("#12345"), None);
        assert_eq!(normalize_hex("#GGGGGG"), None);
        assert!(hex_to_hsla("#FF0000").is_some());
        assert!(hex_to_hsla("炸").is_none());

        let mut colors = TerminalColors::default();
        colors.set_hex_field(CUSTOM_ANSI_BASE + 1, "#XYZ".into());
        colors.set_hex_field(CUSTOM_BRIGHT_BASE + 2, "ff0000".into());
        colors.clamp();
        assert_eq!(
            colors.hex_field(CUSTOM_ANSI_BASE + 1),
            CUSTOM_DEFAULT_ANSI[1]
        );
        assert_eq!(colors.hex_field(CUSTOM_BRIGHT_BASE + 2), "#FF0000");

        // 转成 rgpui-term 配置时前景 / 背景 / 16 色一一对应。
        let config = colors.to_config();
        assert_eq!(config.foreground, colors.foreground);
        assert_eq!(config.background, colors.background);
        assert_eq!(config.ansi, colors.ansi);
        assert_eq!(config.bright, colors.bright);
        // 光标跟随前景，明亮白兼作 bright 前景、普通白作为 dim 前景。
        assert_eq!(config.cursor, colors.foreground);
        assert_eq!(
            config.bright_foreground.as_deref(),
            Some(CUSTOM_DEFAULT_BRIGHT[7])
        );
    }

    #[test]
    fn terminal_prefs_steppers_stop_at_bounds() {
        // 步进夹在合法区间内：连点 + 不会把终端字号撑到窗口放不下，连点 − 不会小到不可读。
        let mut big = TerminalPrefs {
            font_size: FONT_SIZE_MAX,
            ..Default::default()
        };
        big.nudge_font_size(5);
        assert_eq!(big.font_size, FONT_SIZE_MAX);

        let mut small = TerminalPrefs {
            font_size: FONT_SIZE_MIN,
            ..Default::default()
        };
        small.nudge_font_size(-5);
        assert_eq!(small.font_size, FONT_SIZE_MIN);

        let mut dense = TerminalPrefs {
            line_height: LINE_HEIGHT_MIN,
            ..Default::default()
        };
        dense.nudge_line_height(-9);
        assert!((dense.line_height - LINE_HEIGHT_MIN).abs() < 1e-3);

        let mut loose = TerminalPrefs {
            line_height: LINE_HEIGHT_MAX,
            ..Default::default()
        };
        loose.nudge_line_height(9);
        assert!((loose.line_height - LINE_HEIGHT_MAX).abs() < 1e-3);
    }

    #[test]
    fn terminal_prefs_clamp_bad_data_back_into_range() {
        // 库里存的历史/手工改过的越界值一律夹回区间，坏数据不该让终端不可读。
        let huge = TerminalPrefs::from_json(r#"{"font_size":9999.0,"line_height":0.2}"#);
        assert_eq!(huge.font_size, FONT_SIZE_MAX);
        assert!((huge.line_height - LINE_HEIGHT_MIN).abs() < 1e-3);
        assert_eq!(huge.theme, TERMINAL_THEME_DEFAULT);

        let tiny = TerminalPrefs::from_json(r#"{"font_size":1.0,"line_height":99.0}"#);
        assert_eq!(tiny.font_size, FONT_SIZE_MIN);
        assert!((tiny.line_height - LINE_HEIGHT_MAX).abs() < 1e-3);

        // 空方案名回落默认主题，其余内容仍保留。
        let blank = TerminalPrefs::from_json(r#"{"font_size":18.0,"theme":""}"#);
        assert_eq!(blank.font_size, 18.0);
        assert_eq!(blank.theme, TERMINAL_THEME_DEFAULT);

        // 空串与损坏 JSON 回默认（14px / 1.2 / One Dark）。
        assert_eq!(TerminalPrefs::from_json(""), TerminalPrefs::default());
        assert_eq!(
            TerminalPrefs::from_json(r#"{"font_size":"大","line_height":[]}"#),
            TerminalPrefs::default()
        );
    }
}
