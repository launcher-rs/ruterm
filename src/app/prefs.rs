//! //! 应用偏好：状态栏 / 终端 / 会话 / 转发 / 跳板 / 协议 / 本机 shell / 侧栏分组。

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use rgpui::{Hsla, Rgba};
use rgpui_term::TerminalThemeConfig;
use serde::{Deserialize, Serialize};

use crate::remote::Metric;
use crate::sessions::{HostConfig, HostExtras, ProtocolKind};

use super::*;

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
    /// 终端状态读数开关（尺寸 / 光标 / 回滚 / 选区）。
    #[serde(default)]
    pub readout: StatusReadoutPrefs,
}

/// 状态栏里可勾选的一个显示项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StatusPref {
    Addr,
    Auth,
    Metric(Metric),
    Readout(ReadoutKey),
}

/// 状态栏「终端状态」四条目。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadoutKey {
    Size,
    Cursor,
    Scrollback,
    Selection,
}

impl ReadoutKey {
    pub(super) const ALL: [ReadoutKey; 4] =
        [Self::Size, Self::Cursor, Self::Scrollback, Self::Selection];

    /// 设置持久化用的稳定 id。
    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Cursor => "cursor",
            Self::Scrollback => "scrollback",
            Self::Selection => "selection",
        }
    }

    /// 条目标签（设置界面用，中文原文即 tr! key）。
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Size => "终端尺寸（列×行）",
            Self::Cursor => "光标行列",
            Self::Scrollback => "回滚行数（滚动到历史时出现）",
            Self::Selection => "选区行数（选中时出现）",
        }
    }
}

/// 终端状态读数开关，单独成结构：四个条目同属一组，设置分页与序列化都按组走。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReadoutPrefs {
    #[serde(default = "shown")]
    pub size: bool,
    #[serde(default = "shown")]
    pub cursor: bool,
    #[serde(default = "shown")]
    pub scrollback: bool,
    #[serde(default = "shown")]
    pub selection: bool,
}

impl Default for StatusReadoutPrefs {
    fn default() -> Self {
        Self {
            size: true,
            cursor: true,
            scrollback: true,
            selection: true,
        }
    }
}

impl StatusReadoutPrefs {
    pub(super) fn on(&self, key: ReadoutKey) -> bool {
        match key {
            ReadoutKey::Size => self.size,
            ReadoutKey::Cursor => self.cursor,
            ReadoutKey::Scrollback => self.scrollback,
            ReadoutKey::Selection => self.selection,
        }
    }

    pub(super) fn set(&mut self, key: ReadoutKey, on: bool) {
        match key {
            ReadoutKey::Size => self.size = on,
            ReadoutKey::Cursor => self.cursor = on,
            ReadoutKey::Scrollback => self.scrollback = on,
            ReadoutKey::Selection => self.selection = on,
        }
    }

    pub(super) fn any(&self) -> bool {
        ReadoutKey::ALL.iter().any(|key| self.on(*key))
    }
}

impl Default for StatusBarPrefs {
    fn default() -> Self {
        Self {
            addr: true,
            auth: true,
            metrics: HashMap::new(),
            readout: StatusReadoutPrefs::default(),
        }
    }
}

impl StatusBarPrefs {
    /// 该远程指标是否显示（未记录即显示）。
    pub(super) fn metric_on(&self, metric: Metric) -> bool {
        *self.metrics.get(metric.id()).unwrap_or(&true)
    }

    pub(super) fn set_metric(&mut self, metric: Metric, on: bool) {
        self.metrics.insert(metric.id().to_string(), on);
    }

    /// 序列化为入库/入文件的字符串。
    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 解析库内 / settings.json 里的偏好；损坏或为空时回默认（全显示）。
    pub(super) fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 检查更新偏好：默认只连 GitHub Release；镜像是网络不通时的可选中转。
///
/// 镜像前缀按优先级排列，组件会依次尝试自定义 → 内置 → 直连；开关关闭时完全不走镜像。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePrefs {
    /// 是否启用镜像中转。
    #[serde(default)]
    pub use_mirrors: bool,
    /// 自定义镜像前缀（去尾部斜杠后保存）。
    #[serde(default)]
    pub custom_mirrors: Vec<String>,
}

impl Default for UpdatePrefs {
    fn default() -> Self {
        Self {
            // 默认开启：前面几个是实测可用的前缀，最后一个兜底仍是直连。
            // 不信任第三方中转时可在「设置 → 更新」关掉。
            use_mirrors: true,
            custom_mirrors: crate::update::RECOMMENDED_MIRRORS
                .iter()
                .map(|mirror| (*mirror).to_string())
                .collect(),
        }
    }
}

impl UpdatePrefs {
    /// 界面输入框一行写一个前缀；同时兼容旧的空格 / 逗号分隔写法。
    /// 空串与重复项直接丢掉，尾部斜杠统一去掉。
    pub(super) fn set_mirrors_text(&mut self, text: &str) {
        let mut mirrors = Vec::new();
        for line in text.lines() {
            for part in line.split(|c: char| c.is_whitespace() || c == ',') {
                let mirror = part.trim().trim_end_matches('/').to_string();
                if !mirror.is_empty() && !mirrors.contains(&mirror) {
                    mirrors.push(mirror);
                }
            }
        }
        self.custom_mirrors = mirrors;
    }

    /// 回填输入框：一行一个前缀，方便逐条查看与编辑。
    pub(super) fn mirrors_text(&self) -> String {
        self.custom_mirrors.join("\n")
    }

    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(super) fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 终端字号范围与步进：下限保证可读，上限避免把窗口撑爆。
pub(super) const FONT_SIZE_MIN: f32 = 10.0;
pub(super) const FONT_SIZE_MAX: f32 = 32.0;
pub(super) const FONT_SIZE_STEP: f32 = 1.0;
pub(super) const FONT_SIZE_DEFAULT: f32 = 14.0;
/// 行高倍数范围与步进（过密会糊字，过疏浪费竖向空间）。
pub(super) const LINE_HEIGHT_MIN: f32 = 1.0;
pub(super) const LINE_HEIGHT_MAX: f32 = 2.0;
pub(super) const LINE_HEIGHT_STEP: f32 = 0.1;
pub(super) const LINE_HEIGHT_DEFAULT: f32 = 1.2;
/// 配色方案默认值，与 rgpui-term `ThemeManager` 的初始主题一致。
pub(super) const TERMINAL_THEME_DEFAULT: &str = "One Dark";
/// 自定义配色在 `ThemeManager` 里注册的名字，也是 `TerminalPrefs::theme` 的一个取值。
pub(super) const TERMINAL_THEME_CUSTOM: &str = "自定义";
/// 自定义配色可编辑的字段数：前景、背景、8 个普通色、8 个明亮色。
pub(super) const CUSTOM_COLOR_FIELDS: usize = 18;
/// 普通色在 `TerminalColors` 字段序列里的起点（前两个是前景 / 背景）。
pub(super) const CUSTOM_ANSI_BASE: usize = 2;
/// 明亮色起点（紧跟 8 个普通色）。
pub(super) const CUSTOM_BRIGHT_BASE: usize = CUSTOM_ANSI_BASE + 8;
/// ANSI 前 8 色的中文名，按「黑 红 绿 黄 蓝 洋红 青 白」排列。
pub(super) const ANSI_COLOR_NAMES: [&str; 8] = ["黑", "红", "绿", "黄", "蓝", "洋红", "青", "白"];
/// 自定义配色的初始值：与默认方案 One Dark 一致，用户切到「自定义」时不会突然换色。
pub(super) const CUSTOM_DEFAULT_FOREGROUND: &str = "#ABB2BF";
pub(super) const CUSTOM_DEFAULT_BACKGROUND: &str = "#282C34";
pub(super) const CUSTOM_DEFAULT_ANSI: [&str; 8] = [
    "#3F4451", "#E06C75", "#98C379", "#E5C07B", "#61AFEF", "#C678DD", "#56B6C2", "#ABB2BF",
];
pub(super) const CUSTOM_DEFAULT_BRIGHT: [&str; 8] = [
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
    pub(super) fn hex_field(&self, index: usize) -> &str {
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
    pub(super) fn set_hex_field(&mut self, index: usize, value: String) {
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
    pub(super) fn clamp(&mut self) {
        let defaults = Self::default();
        for index in 0..CUSTOM_COLOR_FIELDS {
            let value = self.hex_field(index).to_string();
            let fixed =
                normalize_hex(&value).unwrap_or_else(|| defaults.hex_field(index).to_string());
            self.set_hex_field(index, fixed);
        }
    }

    /// 转成 rgpui-term 的配置；光标跟随前景色，明亮白兼作 bright 前景。
    pub(super) fn to_config(&self) -> TerminalThemeConfig {
        TerminalThemeConfig {
            foreground: self.foreground.clone(),
            background: self.background.clone(),
            cursor: self.foreground.clone(),
            ansi: self.ansi.clone(),
            bright: self.bright.clone(),
            bright_foreground: Some(self.bright[7].clone()),
            dim_foreground: Some(self.ansi[7].clone()),
            ..Default::default()
        }
    }
}

/// 把 `#RGB` / `#RRGGBB`（大小写、省略 `#` 均可）规范成 `#RRGGBB`；不合法返回 None。
pub(super) fn normalize_hex(text: &str) -> Option<String> {
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
pub(super) fn hex_to_hsla(hex: &str) -> Option<Hsla> {
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
    /// 输入命令时自动弹出片段候选（Tab / Enter 补全）。
    #[serde(default = "inline_completion_default")]
    pub inline_completion: bool,
    /// 向远端 shell 注入 OSC 7 目录上报（文件面板「跟随 / 定位」的来源）。
    #[serde(default = "shell_integration_default")]
    pub shell_integration: bool,
    /// 标记用户已明确选择 shell 集成开关；旧配置里的默认开启值需要迁移为关闭。
    #[serde(default)]
    pub(super) shell_integration_configured: bool,
}

pub(super) fn shell_integration_default() -> bool {
    false
}

pub(super) fn inline_completion_default() -> bool {
    true
}

pub(super) fn default_font_size() -> f32 {
    FONT_SIZE_DEFAULT
}

pub(super) fn default_line_height() -> f32 {
    LINE_HEIGHT_DEFAULT
}

pub(super) fn default_terminal_theme() -> String {
    TERMINAL_THEME_DEFAULT.to_string()
}

impl Default for TerminalPrefs {
    fn default() -> Self {
        Self {
            font_size: FONT_SIZE_DEFAULT,
            line_height: LINE_HEIGHT_DEFAULT,
            theme: TERMINAL_THEME_DEFAULT.to_string(),
            custom: TerminalColors::default(),
            inline_completion: true,
            shell_integration: false,
            shell_integration_configured: true,
        }
    }
}

impl TerminalPrefs {
    /// 把数值夹回合法区间：库里存的是历史版本写下的值，坏数据不该让终端不可读。
    pub(super) fn clamp(&mut self) {
        self.font_size = self.font_size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX).round();
        self.line_height = self.line_height.clamp(LINE_HEIGHT_MIN, LINE_HEIGHT_MAX);
        if self.theme.is_empty() {
            self.theme = TERMINAL_THEME_DEFAULT.to_string();
        }
        self.custom.clamp();
    }

    /// 字号增减一档（`+1` / `-1`），结果仍夹在合法区间内。
    pub(super) fn nudge_font_size(&mut self, steps: i32) {
        let next = self.font_size + FONT_SIZE_STEP * steps as f32;
        self.font_size = next;
        self.clamp();
    }

    /// 行高增减一档。
    pub(super) fn nudge_line_height(&mut self, steps: i32) {
        let next = self.line_height + LINE_HEIGHT_STEP * steps as f32;
        self.line_height = next;
        self.clamp();
    }

    /// 序列化为入库/入文件的字符串。
    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 解析库内 / settings.json 里的偏好；损坏或为空时回默认。
    pub(super) fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        let mut prefs: Self = serde_json::from_str(text).unwrap_or_default();
        // 旧版默认开启会在连接后向交互 shell 注入并执行一条命令。旧配置没有
        // 「用户明确选择」标记，因此一律迁移为关闭，避免升级后继续自动注入。
        if !prefs.shell_integration_configured {
            prefs.shell_integration = false;
            prefs.shell_integration_configured = true;
        }
        prefs.clamp();
        prefs
    }
}

/// 连接超时范围（秒）：太短会把慢服务器误判成不通，太长则白等。
pub(super) const CONNECT_TIMEOUT_MIN: u64 = 5;
pub(super) const CONNECT_TIMEOUT_MAX: u64 = 120;
pub(super) const CONNECT_TIMEOUT_STEP: u64 = 5;
pub(super) const CONNECT_TIMEOUT_DEFAULT: u64 = 15;
/// 自动重连最多连续尝试次数：断网时无限重试既吵又费资源，5 次后交给用户手动决定。
pub(super) const AUTO_RECONNECT_MAX: u32 = 5;
/// 会话稳定存活超过这个时长后再次断线，重新计算自动重连次数（避免历史断线把额度吃光）。
pub(super) const AUTO_RECONNECT_HEALTHY: Duration = Duration::from_secs(30);

/// 内嵌私钥文件大小上限：正常私钥几 KB，超过这个数多半是选错了文件。
pub(super) const MAX_INLINE_KEY_BYTES: usize = 64 * 1024;

/// 第 n 次自动重连前的等待：退避避免对刚恢复的网络连打。
pub(super) fn auto_reconnect_delay(attempt: u32) -> Duration {
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

pub(super) fn auto_reconnect_default() -> bool {
    true
}

pub(super) fn connect_timeout_default() -> u64 {
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
    pub(super) fn clamp(&mut self) {
        self.connect_timeout = self
            .connect_timeout
            .clamp(CONNECT_TIMEOUT_MIN, CONNECT_TIMEOUT_MAX);
    }

    /// 超时增减一档，结果仍夹在合法区间内。
    pub(super) fn nudge_connect_timeout(&mut self, steps: i32) {
        let delta = CONNECT_TIMEOUT_STEP as i64 * i64::from(steps);
        let next = (self.connect_timeout as i64 + delta).max(0) as u64;
        self.connect_timeout = next;
        self.clamp();
    }

    /// 序列化为入库/入文件的字符串。
    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 解析库内 / settings.json 里的偏好；损坏或为空时回默认。
    pub(super) fn from_json(text: &str) -> Self {
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
    pub kind: crate::session::ForwardKind,
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

pub(super) fn forward_listen_default() -> u16 {
    8080
}

pub(super) fn forward_host_default() -> String {
    "127.0.0.1".to_string()
}

pub(super) fn forward_port_default() -> u16 {
    80
}

impl SavedForward {
    /// 折算成会话层规则。
    pub fn rule(&self) -> crate::session::ForwardRule {
        crate::session::ForwardRule {
            kind: self.kind,
            listen_port: self.listen_port,
            target_host: self.target_host.clone(),
            target_port: self.target_port,
        }
    }

    /// 列表主标题：有备注用备注，否则用规则描述。
    pub(super) fn title(&self) -> String {
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
    pub(super) rules: HashMap<String, Vec<SavedForward>>,
}

impl ForwardPrefs {
    /// 某连接的规则列表（无记录时返回空切片）。
    pub(super) fn rules_for(&self, addr: &str) -> &[SavedForward] {
        self.rules.get(addr).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 保存一条规则：同方向同监听端口视为同一条，原位替换，避免重复行。
    pub(super) fn insert(&mut self, addr: &str, saved: SavedForward) {
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
    pub(super) fn remove(&mut self, addr: &str, rule_id: &str) {
        let Some(list) = self.rules.get_mut(addr) else {
            return;
        };
        list.retain(|item| item.rule().id() != rule_id);
        if list.is_empty() {
            self.rules.remove(addr);
        }
    }

    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(super) fn from_json(text: &str) -> Self {
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
    pub(super) jumps: BTreeMap<String, i64>,
}

impl JumpPrefs {
    pub(super) fn jump_of(&self, host_id: i64) -> Option<i64> {
        self.jumps.get(&host_id.to_string()).copied()
    }

    pub(super) fn set(&mut self, host_id: i64, jump_id: Option<i64>) {
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
    pub(super) fn forget(&mut self, host_id: i64) -> Vec<i64> {
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

    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(super) fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 主机 → 协议映射（host id → protocol）。
///
/// 为什么不进 hosts 表：同 [`JumpPrefs`]，加密库的表由 toasty 建模，加列要手工
/// ALTER 旧库；协议只记录非 SSH 的项，随 `settings` 表的 `protocols` 键持久化，
/// 启动后回填到 [`HostConfig::protocol`]。降级模式直接随 sessions.json 落盘。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolPrefs {
    /// 键是字符串形式的 host id（JSON 对象的键只能是字符串）。
    #[serde(default)]
    pub(super) protocols: BTreeMap<String, ProtocolKind>,
}

impl ProtocolPrefs {
    pub(super) fn protocol_of(&self, host_id: i64) -> Option<ProtocolKind> {
        self.protocols.get(&host_id.to_string()).copied()
    }

    pub(super) fn set(&mut self, host_id: i64, protocol: ProtocolKind) -> bool {
        let key = host_id.to_string();
        match protocol {
            ProtocolKind::Ssh => self.protocols.remove(&key).is_some(),
            protocol => self.protocols.insert(key, protocol) != Some(protocol),
        }
    }

    pub(super) fn forget(&mut self, host_id: i64) {
        self.protocols.remove(&host_id.to_string());
    }

    /// 迁移时把 JSON 里已有的协议选择转成映射（只收 Telnet 这类非默认值）。
    pub(super) fn from_hosts(hosts: &[HostConfig]) -> Self {
        let mut prefs = Self::default();
        for host in hosts {
            if host.id != 0 && host.protocol != ProtocolKind::Ssh {
                prefs.set(host.id, host.protocol);
            }
        }
        prefs
    }

    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(super) fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 主机 → 协议附加参数映射（host id → 串口参数 / WebSocket 参数）。
///
/// 为什么不进 hosts 表：同 [`ProtocolPrefs`]，加密库的表由 toasty 建模，加列要手工
/// ALTER 旧库；这些参数只有对应协议的主机才有，随 `settings` 表的 `host_extras`
/// 键持久化。降级模式直接随 settings.json 落盘。串口参数里没有秘密，明文 JSON 即可。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostExtrasPrefs {
    /// 键是字符串形式的 host id（JSON 对象的键只能是字符串）。
    #[serde(default)]
    pub(super) extras: BTreeMap<String, HostExtras>,
}

impl HostExtrasPrefs {
    /// 读出某台主机的附加参数；没有记录时按各自协议的默认值。
    pub(super) fn of(&self, host_id: i64) -> HostExtras {
        self.extras
            .get(&host_id.to_string())
            .cloned()
            .unwrap_or_default()
    }

    /// 覆盖一台主机的附加参数；返回是否发生了变化（据此决定是否回写）。
    /// 全默认值视为「没有参数」，直接删键，不给 settings 留垃圾。
    pub(super) fn set(&mut self, host_id: i64, extras: HostExtras) -> bool {
        let key = host_id.to_string();
        if extras == HostExtras::default() {
            return self.extras.remove(&key).is_some();
        }
        let previous = self.extras.insert(key, extras.clone());
        previous != Some(extras)
    }

    pub(super) fn forget(&mut self, host_id: i64) {
        self.extras.remove(&host_id.to_string());
    }

    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(super) fn from_json(text: &str) -> Self {
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
    pub(super) names: HashMap<String, String>,
    /// profile id → 起始工作目录（未设置则用进程当前目录）。
    #[serde(default)]
    pub(super) cwd: HashMap<String, String>,
    /// profile id → 自定义启动参数原文（空格分隔，可带双引号）。
    #[serde(default)]
    pub(super) args: HashMap<String, String>,
    /// profile id → 环境变量原文（每行 KEY=VALUE）。
    #[serde(default)]
    pub(super) env: HashMap<String, String>,
}

impl LocalShellPrefs {
    /// 该 profile 的自定义名（空串视为未设置）。
    pub(super) fn name_of(&self, id: &str) -> Option<&str> {
        self.names
            .get(id)
            .map(String::as_str)
            .filter(|name| !name.is_empty())
    }

    /// 设置别名；空名等于恢复默认（删除别名）。
    pub(super) fn rename(&mut self, id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.names.remove(id);
        } else {
            self.names.insert(id.to_string(), name.to_string());
        }
    }

    /// 该 profile 的起始目录（未设置或空串视为默认）。
    pub(super) fn cwd_of(&self, id: &str) -> Option<&str> {
        self.cwd
            .get(id)
            .map(String::as_str)
            .filter(|dir| !dir.is_empty())
    }

    /// 设置起始目录；空串等于恢复默认（删除该条）。
    pub(super) fn set_cwd(&mut self, id: &str, dir: &str) {
        let dir = dir.trim();
        if dir.is_empty() {
            self.cwd.remove(id);
        } else {
            self.cwd.insert(id.to_string(), dir.to_string());
        }
    }

    /// 解析后的自定义启动参数（原始文本已在保存时校验并规范化）。
    pub(super) fn args_of(&self, id: &str) -> Vec<String> {
        self.args
            .get(id)
            .map(|text| crate::local_shell::split_args(text))
            .unwrap_or_default()
    }

    /// 保存自定义启动参数原文；空串等于不加参数。
    pub(super) fn set_args(&mut self, id: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.args.remove(id);
        } else {
            self.args.insert(id.to_string(), text.to_string());
        }
    }

    /// 该 profile 的环境变量原文（未设置为空串）。
    pub(super) fn env_of(&self, id: &str) -> &str {
        self.env.get(id).map(String::as_str).unwrap_or("")
    }

    /// 解析后的环境变量（保存时已校验，解析不会失败）。
    pub(super) fn env_map_of(&self, id: &str) -> HashMap<String, String> {
        crate::local_shell::parse_env(self.env_of(id)).unwrap_or_default()
    }

    /// 保存环境变量原文；空串等于不注入额外变量。
    pub(super) fn set_env(&mut self, id: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.env.remove(id);
        } else {
            self.env.insert(id.to_string(), text.to_string());
        }
    }

    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(super) fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 主机列表的组内排序方式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarSort {
    /// 手动排序：右键「上移一位 / 下移一位 / 移到组内最前」。
    #[default]
    Manual,
    /// 按展示名（大小写不敏感）。
    Name,
    /// 按最近连接：最近连过的在最前，从未连接的回落到手动序。
    Recent,
}

/// 侧边栏分组（文件夹）偏好：组顺序 + 折叠态 + 主机归属 + 排序方式。
///
/// 整体序列化成一个 JSON 串，存进加密库 `settings` 表的 `sidebar` 键（降级时进
/// settings.json）。刻意不给 `hosts` 表加列：加列要在加密库上做真实 ALTER，
/// 而分组只是展示层元数据，单独一个键即可，风险与迁移成本都低得多。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SidebarPrefs {
    /// 用户分组，按此顺序显示（未分组恒在末尾，不占位）。
    #[serde(default)]
    pub(super) groups: Vec<GroupEntry>,
    /// 主机 id → 组名。组被删除后残留的映射按未分组处理，不报错。
    #[serde(default)]
    pub(super) assign: HashMap<i64, String>,
    /// 「未分组」文件夹是否折叠。
    #[serde(default)]
    pub(super) ungrouped_collapsed: bool,
    /// 内置「本地终端」分组是否折叠。它不入 `groups`，但折叠态同样要跨重启保留。
    #[serde(default)]
    pub(super) local_collapsed: bool,
    /// 主机 id 的手动排序（跨组一条序列，渲染时只取组内相对次序）。
    #[serde(default)]
    pub(super) order: Vec<i64>,
    /// 组内排序方式；收藏桶恒在最前，与排序方式无关。
    #[serde(default)]
    pub(super) sort: SidebarSort,
}

/// 参与排序的一台主机：只带排序需要的字段，便于脱离 UI 单测。
#[derive(Clone, Copy)]
pub(super) struct SortableHost<'a> {
    pub(super) id: i64,
    pub(super) title: &'a str,
    pub(super) favorite: bool,
}

/// 一个分组（文件夹）条目。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupEntry {
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
}

/// 未分组文件夹的名字（不作为 `groups` 条目存在）。
pub(super) const UNGROUPED: &str = "未分组";

impl SidebarPrefs {
    /// 组名列表（不含未分组）。
    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.groups.iter().map(|group| group.name.as_str())
    }

    /// 该主机所属分组；未分组或组已删除时返回 None。
    pub(super) fn group_of(&self, id: i64) -> Option<&str> {
        let name = self.assign.get(&id).map(String::as_str)?;
        self.groups
            .iter()
            .any(|group| group.name == name)
            .then_some(name)
    }

    /// 新建分组；名字为空或已存在时返回 false。
    pub(super) fn add(&mut self, name: &str) -> bool {
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
    pub(super) fn rename(&mut self, from: &str, to: &str) -> bool {
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
    pub(super) fn remove(&mut self, name: &str) {
        self.groups.retain(|group| group.name != name);
        self.assign.retain(|_, current| current != name);
    }

    /// 折叠 / 展开分组。
    pub(super) fn toggle(&mut self, name: &str) {
        if name == UNGROUPED {
            self.ungrouped_collapsed = !self.ungrouped_collapsed;
            return;
        }
        if let Some(group) = self.groups.iter_mut().find(|group| group.name == name) {
            group.collapsed = !group.collapsed;
        }
    }

    /// 折叠 / 展开内置「本地终端」分组。
    pub(super) fn toggle_local(&mut self) {
        self.local_collapsed = !self.local_collapsed;
    }

    /// 展开分组（新主机落进折叠组时调用，避免保存后"看不见"）。
    pub(super) fn expand(&mut self, name: &str) {
        if name == UNGROUPED {
            self.ungrouped_collapsed = false;
            return;
        }
        if let Some(group) = self.groups.iter_mut().find(|group| group.name == name) {
            group.collapsed = false;
        }
    }

    pub(super) fn collapsed(&self, name: &str) -> bool {
        if name == UNGROUPED {
            return self.ungrouped_collapsed;
        }
        self.groups
            .iter()
            .any(|group| group.name == name && group.collapsed)
    }

    /// 把主机移入某分组；`None` 表示未分组。
    pub(super) fn assign_to(&mut self, id: i64, group: Option<&str>) {
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
    pub(super) fn forget(&mut self, id: i64) {
        self.assign.remove(&id);
        self.order.retain(|&item| item != id);
    }

    /// 主机在手动排序里的位置；未登记的主机排到已登记之后。
    pub(super) fn rank_of(&self, id: i64) -> usize {
        self.order
            .iter()
            .position(|&item| item == id)
            .unwrap_or(usize::MAX)
    }

    /// 用当前存活主机补齐排序表：删掉的清出去，新主机按给定顺序追加到尾部。
    pub(super) fn sync_order(&mut self, alive: &[i64]) {
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
    pub(super) fn move_before(&mut self, id: i64, anchor: i64) {
        if id == anchor {
            return;
        }
        self.order.retain(|&item| item != id);
        match self.order.iter().position(|&item| item == anchor) {
            Some(position) => self.order.insert(position, id),
            None => self.order.push(id),
        }
    }

    /// 切换排序方式；返回是否真的发生了变化（用于决定要不要落盘）。
    pub(super) fn set_sort(&mut self, sort: SidebarSort) -> bool {
        if self.sort == sort {
            return false;
        }
        self.sort = sort;
        true
    }

    /// 两台主机的展示顺序：收藏恒在前，同桶内按当前排序方式。
    ///
    /// 手动序里未登记的主机排到最后；`recent` 是最近连接 id 列表（越靠前越新），
    /// 未连过的按手动序垫底，保证列表不会因为连过一台就整体跳动。
    pub(super) fn compare_hosts(
        &self,
        a: SortableHost<'_>,
        b: SortableHost<'_>,
        recent: &[i64],
    ) -> std::cmp::Ordering {
        b.favorite.cmp(&a.favorite).then_with(|| match self.sort {
            SidebarSort::Manual => self.rank_of(a.id).cmp(&self.rank_of(b.id)),
            SidebarSort::Name => a
                .title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then_with(|| self.rank_of(a.id).cmp(&self.rank_of(b.id))),
            SidebarSort::Recent => recent_rank(a.id, recent)
                .cmp(&recent_rank(b.id, recent))
                .then_with(|| self.rank_of(a.id).cmp(&self.rank_of(b.id))),
        })
    }

    /// 序列化为入库/入文件的字符串。
    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 解析库内 / settings.json 里的分组偏好；损坏或为空时回默认。
    pub(super) fn from_json(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        serde_json::from_str(text).unwrap_or_default()
    }
}

/// 主机在最近连接列表里的位次；没连过的排在最后。
fn recent_rank(id: i64, recent: &[i64]) -> usize {
    recent
        .iter()
        .position(|&item| item == id)
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        AUTO_RECONNECT_MAX, CONNECT_TIMEOUT_DEFAULT, CONNECT_TIMEOUT_MAX, CONNECT_TIMEOUT_MIN,
        CUSTOM_ANSI_BASE, CUSTOM_BRIGHT_BASE, CUSTOM_DEFAULT_ANSI, CUSTOM_DEFAULT_BRIGHT,
        FONT_SIZE_MAX, FONT_SIZE_MIN, ForwardPrefs, HostExtrasPrefs, JumpPrefs, LINE_HEIGHT_MAX,
        LINE_HEIGHT_MIN, LocalShellPrefs, ProtocolPrefs, SavedForward, SessionPrefs, SidebarPrefs,
        SidebarSort, SortableHost, StatusBarPrefs, TERMINAL_THEME_DEFAULT, TerminalColors,
        TerminalPrefs, UpdatePrefs, auto_reconnect_delay, hex_to_hsla, normalize_hex,
    };
    use crate::remote::Metric;
    use crate::session::ForwardKind;
    use crate::sessions::{HostExtras, ProtocolKind};

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
        prefs.insert(
            "tester@127.0.0.1:22",
            saved("web", ForwardKind::Local, 8080),
        );
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
        assert_eq!(
            ForwardPrefs::from_json("{ not json"),
            ForwardPrefs::default()
        );
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
    fn protocol_prefs_roundtrip_and_forget() {
        let mut prefs = ProtocolPrefs::default();
        // SSH 是默认值，不落条目：只有 Telnet 需要额外存储。
        assert!(!prefs.set(5, ProtocolKind::Ssh));
        assert!(prefs.set(6, ProtocolKind::Telnet));
        assert!(!prefs.set(6, ProtocolKind::Telnet));
        assert_eq!(prefs.protocol_of(6), Some(ProtocolKind::Telnet));
        assert_eq!(prefs.protocol_of(7), None);
        assert_eq!(ProtocolPrefs::from_json(&prefs.to_json()), prefs);

        // 改回 SSH = 删条目；删主机 = 清引用。
        assert!(prefs.set(6, ProtocolKind::Ssh));
        assert_eq!(prefs.protocol_of(6), None);
        prefs.set(8, ProtocolKind::Telnet);
        prefs.forget(8);
        assert_eq!(prefs.protocol_of(8), None);
        assert_eq!(ProtocolPrefs::from_json(""), ProtocolPrefs::default());
        assert_eq!(
            ProtocolPrefs::from_json("{ not json"),
            ProtocolPrefs::default()
        );
    }

    #[test]
    fn host_extras_prefs_set_forget_and_roundtrip() {
        let mut prefs = HostExtrasPrefs::default();
        let serial = HostExtras {
            serial: Some(crate::sessions::SerialParams {
                baud: 9600,
                ..Default::default()
            }),
            ws: None,
        };
        assert!(prefs.set(6, serial.clone()));
        assert!(!prefs.set(6, serial.clone()));
        assert_eq!(prefs.of(6), serial);
        // 没有记录的主机按全默认读出。
        assert_eq!(prefs.of(7), HostExtras::default());
        assert_eq!(HostExtrasPrefs::from_json(&prefs.to_json()), prefs);

        // 全默认值 = 删键，不留空壳记录。
        assert!(prefs.set(6, HostExtras::default()));
        assert_eq!(prefs.of(6), HostExtras::default());
        prefs.set(8, serial);
        prefs.forget(8);
        assert_eq!(prefs.of(8), HostExtras::default());
        assert_eq!(HostExtrasPrefs::from_json(""), HostExtrasPrefs::default());
        assert_eq!(
            HostExtrasPrefs::from_json("{ not json"),
            HostExtrasPrefs::default()
        );
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
        let mut prefs = StatusBarPrefs {
            addr: false,
            ..StatusBarPrefs::default()
        };
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

        // 内置「本地终端」不是库里的分组，折叠态也要跨重启保留。
        prefs.toggle_local();
        assert!(prefs.local_collapsed);

        let restored = SidebarPrefs::from_json(&prefs.to_json());
        assert_eq!(restored, prefs);
        assert!(restored.local_collapsed);

        // 旧版 JSON 没有该字段：读回默认展开，不能报错或丢其它偏好。
        let old = SidebarPrefs::from_json(r#"{"groups":[{"name":"生产","collapsed":true}]}"#);
        assert!(!old.local_collapsed);
        assert!(old.collapsed("生产"));

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
    fn sidebar_sort_modes_keep_favorites_first() {
        let mut prefs = SidebarPrefs::default();
        prefs.sync_order(&[7, 5, 9, 3]);

        let mut hosts = vec![
            SortableHost {
                id: 9,
                title: "Zulu",
                favorite: false,
            },
            SortableHost {
                id: 5,
                title: "alpha",
                favorite: false,
            },
            SortableHost {
                id: 7,
                title: "Bravo",
                favorite: true,
            },
            SortableHost {
                id: 3,
                title: "Delta",
                favorite: false,
            },
        ];
        let order =
            |prefs: &SidebarPrefs, hosts: &mut Vec<SortableHost<'static>>, recent: &[i64]| {
                hosts.sort_by(|a, b| prefs.compare_hosts(*a, *b, recent));
                hosts.iter().map(|host| host.id).collect::<Vec<_>>()
            };

        // 手动：收藏在前，其余按 order。
        assert_eq!(order(&prefs, &mut hosts, &[]), vec![7, 5, 9, 3]);

        // 名称：大小写不敏感；收藏仍被拉到最前。
        assert!(prefs.set_sort(SidebarSort::Name));
        assert_eq!(order(&prefs, &mut hosts, &[]), vec![7, 5, 3, 9]);

        // 最近：连过的按最近排序，未连过的回落到手动序垫底。
        assert!(prefs.set_sort(SidebarSort::Recent));
        assert_eq!(order(&prefs, &mut hosts, &[9]), vec![7, 9, 5, 3]);

        // 排序方式随偏好一起落盘；没有该字段的旧 JSON 读回手动排序。
        assert_eq!(SidebarPrefs::from_json(&prefs.to_json()), prefs);
        let old = SidebarPrefs::from_json(r#"{"groups":[]}"#);
        assert_eq!(old.sort, SidebarSort::Manual);
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
        prefs.inline_completion = false;
        prefs.shell_integration = false;
        prefs
            .custom
            .set_hex_field(CUSTOM_BRIGHT_BASE + 3, "#123456".into());
        let restored = TerminalPrefs::from_json(&prefs.to_json());
        assert_eq!(restored.font_size, prefs.font_size);
        assert!((restored.line_height - prefs.line_height).abs() < 1e-3);
        assert_eq!(restored.theme, "Dracula");
        assert_eq!(restored.custom, prefs.custom);
        assert!(!restored.inline_completion);
        assert!(!restored.shell_integration);
        // 老配置的 shell_integration=true 是旧版默认值，不代表用户明确选择，
        // 迁移时必须关掉；新版默认也关闭，只有用户在设置里明确勾选才启用。
        let legacy = TerminalPrefs::from_json(r#"{"font_size":16.0}"#);
        assert_eq!(legacy.custom, TerminalColors::default());
        assert!(legacy.inline_completion);
        assert!(!legacy.shell_integration);
        assert!(legacy.shell_integration_configured);

        // 旧版序列化会保存默认 true，迁移也必须关闭它。
        let old_default = TerminalPrefs::from_json(r#"{"shell_integration":true}"#);
        assert!(!old_default.shell_integration);

        // 新版明确选择会保留。
        let mut opted_in = TerminalPrefs::default();
        opted_in.shell_integration = true;
        opted_in.shell_integration_configured = true;
        assert!(TerminalPrefs::from_json(&opted_in.to_json()).shell_integration);
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

    #[test]
    fn update_prefs_parse_and_roundtrip_mirrors() {
        let mut prefs = UpdatePrefs::default();
        assert!(prefs.use_mirrors);
        assert_eq!(
            prefs.custom_mirrors,
            crate::update::RECOMMENDED_MIRRORS
                .iter()
                .map(|mirror| (*mirror).to_string())
                .collect::<Vec<_>>()
        );
        prefs.set_mirrors_text(
            " https://mirror.example/  \nhttps://mirror2.example\n https://mirror.example ",
        );
        assert_eq!(
            prefs.custom_mirrors,
            vec!["https://mirror.example", "https://mirror2.example"]
        );
        assert_eq!(
            prefs.mirrors_text(),
            "https://mirror.example\nhttps://mirror2.example"
        );
        // 旧的单行空格 / 逗号写法仍可解析，保存后统一变成一行一个。
        prefs.set_mirrors_text(" https://mirror.example/ , https://mirror2.example");
        assert_eq!(
            prefs.mirrors_text(),
            "https://mirror.example\nhttps://mirror2.example"
        );
        assert_eq!(UpdatePrefs::from_json(&prefs.to_json()), prefs);

        prefs.set_mirrors_text("  ,  ");
        assert!(prefs.custom_mirrors.is_empty());
        assert_eq!(UpdatePrefs::from_json(""), UpdatePrefs::default());
        assert_eq!(UpdatePrefs::from_json("{ not json"), UpdatePrefs::default());
    }
}
