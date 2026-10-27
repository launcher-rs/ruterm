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
mod vault;

use std::collections::HashMap;
use std::sync::Arc;

use rgpui::components::Select;
use rgpui::components::split_pane::{CollapsiblePane, SplitPane, SplitPaneState};
use rgpui::components::status_bar::{StatusBar, StatusBarItem};
use rgpui::input_ui::{Input, InputContentType, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::title_bar::TITLE_BAR_HEIGHT;
use rgpui::{
    AbsoluteLength, AnyElement, App, Bounds, Button, ButtonVariants, Checkbox, ClickEvent, Context,
    ContextMenuExt, Div, DropdownMenu, Entity, FocusHandle, Focusable, FontWeight, Hsla, Icon,
    IconName, Image, ImageFormat, InteractiveElement, IntoElement, KeyBinding, MouseButton,
    NoAction, ParentElement, Pixels, PopupMenu, PopupMenuItem, Render, ScrollableElement,
    SharedString, Stateful, StatefulInteractiveElement, Styled, TrayMenuItem, Window, WindowBounds,
    WindowControlArea, WindowOptions, actions, div, h_flex, img, px, size, v_flex,
};
use rgpui_term::{Event as TerminalEvent, TerminalBuilder, TerminalView, TextStyle, ThemeManager};
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
        OpenSettings
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
}

/// 新建/编辑主机表单（各字段为受控输入状态）。
struct NewHostForm {
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    key_path: Entity<InputState>,
    /// 加密私钥口令（可选）。保存时勾选入库则写加密库，否则仅本次内存缓存；绝不落明文。
    passphrase: Entity<InputState>,
    /// 表单层"保存口令到加密库"开关（默认否＝仅本次内存持有）。
    save_secret: bool,
    auth: AuthMethod,
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

/// 终端外观偏好：字号、行高与配色方案（方案名取 `ThemeManager` 预设）。
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
    /// 配色方案名（`ThemeManager::available_themes()` 之一）。
    #[serde(default = "default_terminal_theme")]
    pub theme: String,
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
    /// 侧边栏分组（文件夹）偏好，随加密库 / settings.json 双层持久化。
    sidebar: SidebarPrefs,
    /// 分组新建 / 重命名弹窗（`renaming` 为 None 表示新建）。
    group_prompt: Option<GroupPrompt>,
    /// 删除二次确认（真实下标；浮层遮挡期间列表不会变）。
    confirm_delete: Option<usize>,
    /// 主密码解锁弹窗（便携模式 / 已设置主密码时启动弹出）。
    unlock: Option<UnlockState>,
    /// 设置弹窗（主密码管理、主题、锁定）。
    settings: Option<SettingsState>,
    /// 本次运行内缓存的密码（按 user@host:port），避免重复弹窗；绝不落盘。
    password_cache: HashMap<String, String>,
    /// 标签自增 id。
    next_id: usize,
    /// 状态栏左侧的瞬态消息（连接/保存/删除/断开等节点更新）。
    status: SharedString,
    /// 状态栏显示项偏好（勾选哪些条目），随加密库 / settings.json 持久化。
    status_bar: StatusBarPrefs,
    /// 终端外观偏好（字号 / 行高 / 配色方案），同样双层持久化。
    terminal: TerminalPrefs,
    /// 侧边栏 ↔ 主区 的分隔面板状态（拖拽调整宽度 + 折叠/展开）。
    split_state: Entity<SplitPaneState>,
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
        let sidebar = SidebarPrefs::from_json(&settings.sidebar);
        // 终端配色预设管理器：全局一份，`TerminalView::set_theme` 与自建样式都从这里取。
        if !cx.has_global::<ThemeManager>() {
            cx.set_global(ThemeManager::new(None));
        }
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
            search,
            tabs: Vec::new(),
            active_tab: None,
            new_host: None,
            pending: None,
            local_shells: local_shell::detect(),
            shell_icons: HashMap::new(),
            scanning_shells: false,
            local_group_collapsed: false,
            sidebar,
            group_prompt: None,
            confirm_delete: None,
            unlock: None,
            settings: None,
            unlocked_key: None,
            password_cache: HashMap::new(),
            next_id: 0,
            status: "就绪".into(),
            status_bar,
            terminal,
            split_state,
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
                    if this.terminal != prefs {
                        this.terminal = prefs;
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

    /// 打开新建主机表单。
    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form_at(None, window, cx);
    }

    /// 打开新建 / 编辑表单；edit_index 为 Some 时以该主机字段预填。
    fn open_form_at(
        &mut self,
        edit_index: Option<usize>,
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
            save_secret: false,
            auth,
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
        let host_raw = form.host.read(cx).value().trim().to_string();
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
            key_path: form.key_path.read(cx).value().trim().to_string(),
            favorite: false,
        };
        if let Some(error) = host.validate() {
            log::warn!("保存主机失败: {error}");
            self.status = SharedString::from(format!("保存失败：{error}"));
            cx.notify();
            return;
        }
        let label = if host.name.is_empty() {
            host.host.clone()
        } else {
            host.name.clone()
        };
        if let Some(index) = editing {
            // 原位替换：id 与收藏标记沿用原主机；已保存的密码/口令不受影响。
            if let Some(current) = self.hosts.get(index).cloned() {
                host.id = current.id;
                host.favorite = current.favorite;
                self.hosts[index] = host.clone();
                self.persist_host(&host, cx);
                let note = self.store_form_secret(&host, passphrase.clone(), save_secret, cx);
                self.new_host = None;
                self.status = SharedString::from(format!(
                    "已更新主机 {label}{}",
                    note.as_deref().unwrap_or("")
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
        let note = self.store_form_secret(&host, passphrase, save_secret, cx);
        self.new_host = None;
        self.status = SharedString::from(format!(
            "已保存主机 {label}{}",
            note.as_deref().unwrap_or("")
        ));
        cx.notify();
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

    /// 把主机移入分组（`None` = 未分组）。
    fn move_host(&mut self, index: usize, group: Option<SharedString>, cx: &mut Context<Self>) {
        let Some(id) = self.hosts.get(index).map(|host| host.id) else {
            return;
        };
        self.sidebar.assign_to(id, group.as_deref());
        cx.notify();
        self.persist_sidebar(cx);
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
                let target = SshTarget::from_host(&host, None);
                let title = host.name.clone();
                self.connect(title, target, None, None, false, window, cx);
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
                let _ = cx.with_window(entity_id, move |window, cx| {
                    let _ = entity.update(cx, move |this, cx| {
                        if this.pending.is_some() {
                            return;
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
            AuthMethod::Password => SshTarget::from_host(&host, Some(secret)),
            _ => SshTarget::from_host(&host, None).with_passphrase(Some(secret)),
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
        let target = SshTarget::from_host(&host, None);
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
            PromptKind::Password => SshTarget::from_host(&host, Some(secret)),
            PromptKind::Passphrase => {
                SshTarget::from_host(&host, None).with_passphrase(Some(secret))
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
        let terminal_view = cx.new(|cx| TerminalView::new_with_style(terminal, style, window, cx));
        // 状态栏展示用的连接位置与认证方式，须在 target 被 session::start 消费前取出。
        let addr = format!("{}@{}:{}", target.user, target.host, target.port);
        let auth = if target.password.is_some() {
            AuthMethod::Password
        } else if target.key_path.is_some() {
            AuthMethod::IdentityFile
        } else {
            AuthMethod::Agent
        };
        let id = self.next_id;
        self.next_id += 1;
        let session::SshSession {
            join,
            remote_info,
            started,
        } = session::start(target, handles, self.vault.clone(), cx);
        self.status = SharedString::from(format!("正在连接 {addr}…"));
        self.tabs.push(SessionTab {
            id,
            title: title.into(),
            addr: addr.clone().into(),
            auth: Some(auth),
            remote_info: None,
            terminal_view,
        });
        self.active_tab = Some(id);
        let vault = self.vault.clone();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let shell_started = started.load(std::sync::atomic::Ordering::SeqCst);
            let Some(entity) = this.upgrade() else {
                return;
            };
            let entity_id = entity.entity_id();
            let _ = cx.with_window(entity_id, move |window, cx| {
                let _ = entity.update(cx, move |this, cx| {
                    let Err(error) = result else {
                        this.remove_tab(id, cx);
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
                    if !shell_started {
                        // 从未建立终端：保留标签页，错误详情已写入终端可继续阅读。
                        this.status = SharedString::from(format!("连接失败：{addr}"));
                        cx.notify();
                        return;
                    }
                    this.remove_tab(id, cx);
                    this.status = SharedString::from(format!(
                        "连接失败：{addr}（{}）",
                        session::error_summary(&error)
                    ));
                    cx.notify();
                });
            });
        })
        .detach();
        // 远程基本信息比会话短得多：探测回来时标签可能已关闭，据此判断后再写入。
        cx.spawn(async move |this, cx| {
            if let Ok(info) = remote_info.await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == id) {
                        tab.remote_info = Some(info);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        cx.notify();
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
        self.tabs.retain(|tab| tab.id != id);
        if self.tabs.len() == before {
            return;
        }
        if let Some(title) = title {
            self.status = SharedString::from(format!("已断开 {title}"));
        }
        if self.active_tab == Some(id) {
            self.active_tab = self.tabs.last().map(|tab| tab.id);
        }
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
        let Some(program) = chosen.as_ref().map(|profile| profile.program.clone()) else {
            self.status = "未找到可用的本地 shell".into();
            cx.notify();
            return;
        };
        let label = chosen
            .as_ref()
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        let working_directory = std::env::current_dir().ok();
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
            HashMap::new(),
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
        });
        self.active_tab = Some(id);
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
            .on_action(cx.listener(Self::open_settings_action))
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

        // 会话菜单：新建主机、本地终端、侧栏折叠、设置入口、托盘与退出。
        // 前四项挂 action（而非 on_click）：rgpui 会按键绑定在项右侧自动显示
        // Ctrl+N / Ctrl+Shift+T / Ctrl+B / Ctrl+,。
        let session_menu = move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
            menu.item(PopupMenuItem::new("新建主机…").action(Box::new(NewHost)))
                .item(PopupMenuItem::new("本地终端…").action(Box::new(NewLocalTerminal)))
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
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .pr_1()
                    // 口令档且未解锁：只留重新解锁入口（其余操作都需要库）。
                    .when(locked, |this| {
                        this.child(
                            Button::new("unlock-vault")
                                .label("🔑")
                                .compact()
                                .ghost()
                                .tooltip("输入主密码解锁凭据库")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_unlock_dialog(window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("open-settings")
                            .label("⚙")
                            .compact()
                            .ghost()
                            .tooltip("设置（凭据库 / 外观分页）")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings(window, cx);
                            })),
                    )
                    // 已设主密码才出现锁定按钮：锁定后需重新输入口令才能用库。
                    .when(passphrase_mode && !locked, |this| {
                        this.child(
                            Button::new("lock-vault")
                                .label("🔒")
                                .compact()
                                .ghost()
                                .tooltip("锁定凭据库（需主密码解锁）")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.lock_vault(window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("sidebar-collapse")
                            .label("«")
                            .compact()
                            .ghost()
                            .tooltip("折叠侧边栏 (Ctrl+B)")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.split_state.update(cx, |state, cx| {
                                    state.collapse(CollapsiblePane::First, cx);
                                });
                                cx.notify();
                            })),
                    ),
            )
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

    /// 设置居中弹窗：左侧竖排分页导航（凭据库与主密码 / 外观 / 状态栏 / 终端），右侧为当前分页内容。
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
                        this.open_form_at(Some(index), window, cx);
                    },
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
                |this, window, cx| {
                    this.open_form(window, cx);
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
            || local_shell::matches(&self.local_shells, query);
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
        let rows: Vec<AnyElement> = self
            .local_shells
            .iter()
            .filter(|profile| profile.matches(query))
            .enumerate()
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
        let name = profile.name.clone();
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
            menu.item(
                PopupMenuItem::new(format!("打开 {name}")).on_click(move |_, _, cx| {
                    opening.update(cx, |this, cx| {
                        let chosen = this.local_shells.get(index).cloned();
                        this.open_local_terminal(chosen.as_ref(), cx);
                    });
                }),
            )
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
        // 收藏主机置顶（稳定排序，同组内保持原插入顺序）。
        matched.sort_by_key(|&index| !self.hosts[index].favorite);
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
        // 首启动（既没主机也没建过分组）：用一个提示代替空壳「未分组」文件夹。
        let bare = self.hosts.is_empty() && !searching && buckets.len() == 1;
        let mut shown = 0usize;
        if !bare {
            for (name, rows) in &buckets {
                // 搜索时：空组不显示，折叠也照常展开；平时：空组仍显示，便于往里挪主机。
                if searching && rows.is_empty() {
                    continue;
                }
                let collapsed = !searching && self.sidebar.collapsed(name);
                shown += 1;
                list = list.child(self.render_group_row(name, rows.len(), collapsed, &pal, cx));
                if collapsed {
                    continue;
                }
                list = list.children(
                    rows.iter()
                        .map(|&index| self.render_host_row(index, &pal, cx)),
                );
            }
        }
        if shown == 0 && !(searching && local_hit) {
            let hint = if bare {
                "暂无主机：在列表空白处点右键选「新建主机…」，或用「会话」菜单 / Ctrl+N。"
            } else {
                "无匹配主机"
            };
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(hint),
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
                            .child(
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
                if pending.host.key_path.is_empty() {
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
            div()
        } else {
            h_flex()
                .h(px(44.0))
                .flex_shrink_0()
                .items_center()
                .bg(pal.tab_bar_bg)
                .border_b_1()
                .border_color(pal.border)
                .px_3()
                .gap_2()
                .overflow_hidden()
                .children(self.tabs.iter().map(|tab| {
                    let id = tab.id;
                    let active = self.active_tab == Some(id);
                    let tab_id = SharedString::from(format!("tab-{id}"));
                    let close_id = SharedString::from(format!("tab-close-{id}"));
                    h_flex()
                        .id(tab_id)
                        .h(px(32.0))
                        .min_w(px(160.0))
                        .flex_shrink_0()
                        .px_4()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .rounded(px(6.0))
                        // 活动/非活动只差底色与描边不足以分辨（浅色主题下 tab_bar 与
                        // tab_active 仅隔一档灰），再给活动标签一圈强调色描边。
                        .bg(if active {
                            pal.tab_active_bg
                        } else {
                            Hsla::transparent_black()
                        })
                        .border_1()
                        .border_color(if active { pal.accent } else { pal.border })
                        .text_color(if active {
                            pal.tab_active_fg
                        } else {
                            pal.tab_fg
                        })
                        .cursor_pointer()
                        .hover(|this| this.bg(pal.card_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.active_tab = Some(id);
                            cx.notify();
                        }))
                        .child(div().flex_1().min_w_0().truncate().child(tab.title.clone()))
                        .child(
                            div()
                                .id(close_id)
                                .text_base()
                                .text_color(pal.danger)
                                .child("×")
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.close_tab(id, cx)),
                                ),
                        )
                }))
        };

        let terminal_bg = self.terminal_bg(cx);
        let body = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .map(|tab| {
                // 内边距取终端自己的底色，四周才像终端的一部分而不是白框。
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(terminal_bg)
                    .px_3()
                    .py_2()
                    .child(tab.terminal_view.clone())
                    .into_any_element()
            })
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
            });

        // 侧边栏折叠后，主区顶部补一条展开栏（分隔面板自带折叠按钮已关闭）。
        let collapsed = self.split_state.read(cx).is_collapsed();
        let expand_bar = if collapsed {
            h_flex()
                .h(px(44.0))
                .flex_shrink_0()
                .items_center()
                .px_2()
                .bg(pal.sidebar_bg)
                .border_b_1()
                .border_color(pal.border)
                .child(
                    Button::new("sidebar-expand")
                        .label("»")
                        .ghost()
                        .tooltip("展开侧边栏 (Ctrl+B)")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.split_state.update(cx, |state, cx| state.expand(cx));
                            cx.notify();
                        })),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        v_flex()
            .flex_1()
            .h_full()
            .child(expand_bar)
            .child(tab_bar)
            .child(body)
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

    /// 分组偏好落库；加密库不可用时退回旧 settings.json。
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
            KeyBinding::new("ctrl-,", OpenSettings, None),
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

        // 系统托盘：品牌图标 + 悬停提示 + 右键菜单（显示主界面 / 退出）。
        // 图标以 32px PNG 内嵌，Windows 走 image 解码路径最稳；不依赖运行时文件路径。
        const TRAY_ICON: &[u8] = include_bytes!("../assets/icons/tray-32.png");
        cx.set_tray_icon(Some(TRAY_ICON));
        cx.set_tray_tooltip("RuLink 拾连");
        cx.set_tray_menu(vec![
            TrayMenuItem::Action {
                label: "显示主界面".into(),
                id: "show".into(),
            },
            TrayMenuItem::Separator,
            TrayMenuItem::Action {
                label: "退出".into(),
                id: "quit".into(),
            },
        ]);
        cx.on_tray_menu_action(move |id: SharedString, cx: &mut App| match id.as_ref() {
            "quit" => cx.quit(),
            "show" => {
                // 从托盘恢复：把隐藏的主窗口重新带到前台。
                window_handle
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
            }
            _ => {}
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{
        FONT_SIZE_MAX, FONT_SIZE_MIN, LINE_HEIGHT_MAX, LINE_HEIGHT_MIN, Metric, SidebarPrefs,
        StatusBarPrefs, TERMINAL_THEME_DEFAULT, TerminalPrefs,
    };

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
    fn terminal_prefs_roundtrip_keeps_all_three_fields() {
        let mut prefs = TerminalPrefs::default();
        prefs.nudge_font_size(3);
        prefs.nudge_line_height(2);
        prefs.theme = "Dracula".into();
        let restored = TerminalPrefs::from_json(&prefs.to_json());
        assert_eq!(restored.font_size, prefs.font_size);
        assert!((restored.line_height - prefs.line_height).abs() < 1e-3);
        assert_eq!(restored.theme, "Dracula");
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
