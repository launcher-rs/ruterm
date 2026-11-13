//! 应用层：RuTermApp 状态、生命周期（建库 / 解锁 / 挂载）与根渲染调度。
//! 功能实现按模块拆在同目录：tabs / connect / files / forwards / hosts / settings 等。

mod appearance;
mod connect;
mod files;
mod forwards;
mod history_ops;
mod history_ui;
mod host_form;
mod hosts;
mod local_files;
mod local_shells;
mod macros_ui;
mod prefs;
mod remote_desktop;
mod settings;
mod sidebar;
mod snippets_ui;
mod status_bar;
mod tabs;
mod terminal_ops;
mod title_bar;
mod vault_ui;
mod widgets;

use connect::*;
use files::*;
use forwards::*;
use history_ui::*;
use host_form::*;
use hosts::*;
use local_files::*;
use local_shells::*;
use macros_ui::*;
use prefs::*;
use remote_desktop::*;
use settings::*;
use sidebar::*;
use snippets_ui::*;
use status_bar::*;
use tabs::*;
use terminal_ops::*;
use vault_ui::*;
use widgets::*;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use rgpui::components::split_pane::{SplitPane, SplitPaneState};
use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::tabs::tab_drag::TabDragData;
use rgpui::{
    AnyElement, App, Button, ButtonVariants, ClickEvent, Context, Entity, FocusHandle, Focusable, Hsla, Icon, IconName, Image, InteractiveElement, IntoElement, MouseButton, ParentElement, Point,
    Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Tooltip, Window, canvas, div, h_flex, px, relative, v_flex,
};
use rgpui_term::{
    InputOrigin,
    ThemeManager,
};

use crate::config::SshTarget;
use crate::history::CommandHistory;
use crate::local_shell::ShellProfile;
use crate::macros::KeyMacro;
use crate::paths::StorageMode;
use crate::sessions::{AuthMethod, HostConfig, ProtocolKind, SessionStore};
use crate::snippets::CommandSnippet;
use crate::vault::Vault;
use crate::tr;

/// RuTerm 主视图：左侧主机账号管理，右侧会话标签终端。
pub(crate) struct RuTermApp {
    /// 会话仓库（负责持久化）。
    store: SessionStore,
    /// 存储模式（便携 / 全局），启动时探测一次。
    mode: StorageMode,
    /// 加密凭据库；None 表示启动引导中、已降级为旧 JSON 存储，或被用户锁定。
    vault: Option<Arc<Vault>>,
    auth_gate: crate::vault::AuthGate,
    pending_quick: Option<SshTarget>,
    vault_opening: bool,
    vault_maintenance: bool,
    /// 本次解锁所用的库主密钥（十六进制）。只存活在内存，绝不入库、入日志、入命令行；
    /// 换主密码前用它校验"当前主密码"，锁定/退出即丢弃。
    unlocked_key: Option<String>,
    /// 已保存的主机账号列表。
    hosts: Vec<HostConfig>,
    /// 最近成功连接的加密库主机 id（新的在前），供「会话」菜单快速重连。
    recent_hosts: Vec<i64>,
    /// 主机列表搜索框（按展示名 / user@host:port 子串过滤）。
    search: Entity<InputState>,
    /// 空工作区首页的主机搜索框（与边栏搜索互不影响）。
    home_search: Entity<InputState>,
    /// 终端内查找条（Ctrl+F）。
    find: FindBar,
    /// 终端右键菜单（复制 / 粘贴 / 全选 / 清屏）。菜单是独立图层里的实体，
    /// `None` 表示当前没有打开。
    terminal_menu: Option<TerminalMenu>,
    /// 同步输入开关：活动终端的按键同时写进其它会话（仅本次运行有效）。
    sync_input: bool,
    /// 终端输入镜像通道（中间件 → UI 分发循环）。
    mirror_tx: tokio::sync::mpsc::UnboundedSender<(usize, InputOrigin, Vec<u8>)>,
    /// 命令广播弹窗（一次性把命令发到所有会话）。
    broadcast: Option<BroadcastState>,
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
    /// 局域网扫描到的主机（不落库；加入列表由用户点选）。
    discovered: Vec<crate::discovery::DiscoveredHost>,
    /// 正在扫描局域网。
    discovering: bool,
    /// 至少完成过一次扫描（决定空结果是否给提示）。
    discovery_scanned: bool,
    /// 扫描失败原因（没有私有网段、系统命令不可用等）。
    discovery_error: Option<SharedString>,
    /// 「局域网发现」弹窗是否打开（入口在「会话」菜单）。
    show_discovery: bool,
    /// 本机 shell 的自定义展示名，随加密库 / settings.json 双层持久化。
    local_shell_prefs: LocalShellPrefs,
    /// 本地 shell 重命名弹窗。
    shell_prompt: Option<ShellPrompt>,
    /// 标签栏横向滚动句柄：新建 / 切到标签时把它滚进可视区。
    tab_scroll: ScrollHandle,
    /// 正在就地重命名的标签（标签 id + 输入框）；回车或失焦提交，Esc 取消。
    renaming: Option<(usize, Entity<InputState>)>,
    /// 文件拖放的落点提示（目标目录 + 文件数）。由 `drag_over` 回调在 paint 阶段写、
    /// 渲染阶段取走：paint 总在 render 之后，所以光标一离开落点，下一帧提示就消失，
    /// 不用额外清理。用 `Cell` 是因为文件页渲染只借到 `&self`。
    drop_hint: Cell<Option<(SharedString, usize)>>,
    /// 侧边栏分组（文件夹）偏好，随加密库 / settings.json 双层持久化。
    sidebar: SidebarPrefs,
    /// 主机 → 跳板机映射（host id → jump id），双层持久化。
    jumps: JumpPrefs,
    /// 主机 → 协议映射（host id → protocol），双层持久化。
    protocols: ProtocolPrefs,
    /// 分组新建 / 重命名弹窗（`renaming` 为 None 表示新建）。
    group_prompt: Option<GroupPrompt>,
    /// 删除二次确认（真实下标；浮层遮挡期间列表不会变）。
    confirm_delete: Option<usize>,
    /// 主密码解锁弹窗（便携模式 / 已设置主密码时启动弹出）。
    unlock: Option<UnlockState>,
    /// 设置弹窗（主密码管理、主题、锁定）。
    settings: Option<SettingsState>,
    /// 边栏当前页面（主机 / 文件 / 本机 / 转发）。
    sidebar_page: SidebarPage,
    /// 本机文件页的浏览状态（当前目录、条目、排序与列宽、隐藏文件开关）。
    local: LocalBrowser,
    /// 正在拖拽的文件列宽（起点指针位置 + 起始宽度）。
    col_drag: Option<ColumnDragState>,
    /// 文件面板的新建目录 / 重命名弹窗。
    file_prompt: Option<FilePrompt>,
    /// 文件面板删除确认。
    file_confirm: Option<FileConfirm>,
    /// 本地副本改动后等待用户确认是否回传（一次显示一个）。
    file_upload_prompt: Option<FileUploadPrompt>,
    /// 其余等待确认 / 回传的文件；按巡检节奏逐个推进。
    file_upload_queue: Vec<FileUploadPrompt>,
    /// 「全部是」授权：仅对授权过的 (标签, 远端路径) 免确认，换文件仍要弹窗。
    file_upload_all: HashSet<(usize, String)>,
    /// 传输（上传/下载）的醒目瞬态提示；数秒后自动淡出，回落到普通 status。
    transfer_flash: Option<TransferFlash>,
    /// 传输提示序号：过期定时器只清掉自己那一条，不误删更新的提示。
    transfer_flash_seq: u64,
    /// 文件面板路径编辑输入框（点击路径栏时激活）：连同它归属的面板一起存，
    /// 本机页和远端页各自认领，不会在 A 页输入、回车却跳到 B 页。
    path_edit: Option<(PaneTarget, Entity<InputState>)>,
    /// 边栏分隔条拖拽起点（指针 x、起点面板占比）。
    split_drag: Option<(f32, f32)>,
    /// 分隔容器宽度（canvas 回填）：拖拽时把指针位移换算成占比。
    split_width: Rc<Cell<f32>>,
    /// 端口转发（内联在边栏的「转发」页里）。
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
    /// 上一次刷进状态栏的终端读数，只用于判断「要不要重绘」（渲染时现读终端）。
    readout_seen: Option<StatusReadout>,
    /// 终端外观偏好（字号 / 行高 / 配色方案 / 自定义 16 色），同样双层持久化。
    terminal: TerminalPrefs,
    /// 自定义配色的 18 个十六进制输入框（前景 / 背景 / 普通 8 色 / 明亮 8 色）。
    custom_color_inputs: Vec<Entity<InputState>>,
    /// 会话行为偏好（自动重连 / 连接超时），同样双层持久化。
    session_prefs: SessionPrefs,
    /// 端口转发规则（按连接保存，开关控制启停），同样双层持久化。
    forward_prefs: ForwardPrefs,
    /// 键宏列表（触发键 + 步骤序列），同样双层持久化。
    macros: Vec<KeyMacro>,
    /// 键宏管理器弹窗（列表 ⇄ 编辑表单）。
    macro_manager: Option<MacroManager>,
    /// 命令片段列表（名称 + 单行命令 + 备注），同样双层持久化。
    snippets: Vec<CommandSnippet>,
    /// 命令片段管理器弹窗（列表 ⇄ 编辑表单）。
    snippet_manager: Option<SnippetManager>,
    /// 命令历史（新 → 旧），来源是终端 block 追踪。
    history: CommandHistory,
    /// 每个标签已收进历史的最后一个 block id，避免重复收录。
    history_seen: HashMap<usize, usize>,
    /// 命令历史弹窗。
    history_manager: Option<HistoryManager>,
    /// 侧边栏 ↔ 主区 的分隔面板状态（拖拽调整宽度 + 折叠/展开）。
    split_state: Entity<SplitPaneState>,
    /// 主区分屏（两个标签并排）；None＝单屏显示活动标签。
    view_split: Option<SplitView>,
    focus_handle: FocusHandle,
}

/// 主题调色板：从 rgpui 全局 `Theme` 的语义色映射而来（不硬编码颜色）。
/// 切换明暗只调 `Theme::change`，本结构随之重取，界面整体换色，且与按钮 / 状态栏等
/// 组件共用同一份主题（避免手工配色与框架主题脱节）。
#[derive(Clone, Copy)]
pub(super) struct Palette {
    /// 窗口/编辑区底色。
    pub(super) window_bg: Hsla,
    /// 侧边栏与标签栏底色。
    pub(super) sidebar_bg: Hsla,
    /// 卡片/表单/非活动标签底色。
    pub(super) card_bg: Hsla,
    /// 卡片悬停底色。
    pub(super) card_hover: Hsla,
    /// 分隔边框色。
    pub(super) border: Hsla,
    /// 主文字色。
    pub(super) text: Hsla,
    /// 次要/弱化文字色。
    pub(super) muted: Hsla,
    /// 认证方式链接色。
    pub(super) link: Hsla,
    /// 删除/关闭等危险操作色。
    pub(super) danger: Hsla,
    /// 选中/主按钮强调色。
    pub(super) accent: Hsla,
    /// 未选中分段（认证方式切换）底色。
    pub(super) segment_bg: Hsla,
    /// 输入框底色。
    pub(super) input_bg: Hsla,
    /// 输入框文字色。
    pub(super) input_text: Hsla,
    /// 输入框边框色。
    pub(super) input_border: Hsla,
    /// 标签栏底色（与编辑区、非活动标签区分）。
    pub(super) tab_bar_bg: Hsla,
    /// 活动标签底色。
    pub(super) tab_active_bg: Hsla,
    /// 活动标签文字色。
    pub(super) tab_active_fg: Hsla,
    /// 非活动标签文字色。
    pub(super) tab_fg: Hsla,
}

impl Palette {
    /// 从当前全局主题取语义色，组装一次性调色板。
    pub(super) fn from_theme(cx: &App) -> Self {
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

impl RuTermApp {
    /// 载入已保存主机，可选执行命令行快速连接。
    pub(crate) fn new(quick: Option<SshTarget>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let protocols = ProtocolPrefs::from_json(&settings.protocols);
        let macros = crate::macros::from_json(&settings.macros);
        let snippets = crate::snippets::from_json(&settings.snippets);
        let history = CommandHistory::from_json(&settings.history);
        let local_files = LocalFilesPrefs::from_json(&settings.local_files);
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
        // 侧边栏 ↔ 主区 分隔面板：默认边栏约占 0.32，够放下文件表格的五列。
        let split_state = cx.new(SplitPaneState::new);
        // 边栏在右时「第一面板」是终端：比例翻过来，边栏仍占约 0.32。
        let sidebar_ratio = default_sidebar_ratio(sidebar.side);
        split_state.update(cx, |state, cx| {
            state.set_ratio(sidebar_ratio, cx);
            // WindTerm 式拖动：拖到最小宽度即停住，分隔线仍可抓住往大拖；
            // 完全收起交给 Ctrl+B，不靠拖拽阈值自动折叠。
            state.set_min_first(260.0, cx);
            state.set_min_second(260.0, cx);
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "搜索主机…")));
        cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        let home_search =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "搜索主机或地址…")));
        cx.subscribe(&home_search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        let find_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!(cx, "查找终端内容…（回车下一个，Shift+回车上一个）"))
        });
        cx.subscribe(&find_input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => this.apply_find(cx),
            InputEvent::PressEnter { shift, .. } => this.step_find(*shift, cx),
            _ => {}
        })
        .detach();
        // 中间件跑在终端内部（按键路径上），投递到 tokio 通道后由 UI 侧分发：
        // 终端实体之间不能互相直接写，否则借用与通知顺序会互相踩。
        let (mirror_tx, mut mirror_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = Self {
            store,
            mode,
            vault: None,
            auth_gate: crate::vault::AuthGate::default(),
            pending_quick: quick,
            vault_opening: false,
            vault_maintenance: false,
            hosts,
            recent_hosts: Vec::new(),
            search,
            home_search,
            find: FindBar {
                input: find_input,
                visible: false,
                regex: false,
                applied: None,
            },
            terminal_menu: None,
            sync_input: false,
            mirror_tx,
            broadcast: None,
            tabs: Vec::new(),
            active_tab: None,
            new_host: None,
            pending: None,
            local_shells: crate::local_shell::detect(),
            shell_icons: HashMap::new(),
            scanning_shells: false,
            discovered: Vec::new(),
            discovering: false,
            discovery_scanned: false,
            discovery_error: None,
            show_discovery: false,
            local_shell_prefs,
            shell_prompt: None,
            tab_scroll: ScrollHandle::new(),
            renaming: None,
            drop_hint: Cell::new(None),
            sidebar,
            jumps,
            protocols,
            group_prompt: None,
            confirm_delete: None,
            unlock: None,
            settings: None,
            sidebar_page: SidebarPage::Hosts,
            col_drag: None,
            file_prompt: None,
            file_confirm: None,
            file_upload_prompt: None,
            file_upload_queue: Vec::new(),
            file_upload_all: HashSet::new(),
            transfer_flash: None,
            transfer_flash_seq: 0,
            path_edit: None,
            local: LocalBrowser::from_prefs(&local_files),
            split_drag: None,
            split_width: Rc::new(Cell::new(0.0)),
            forward: None,
            unlocked_key: None,
            password_cache: HashMap::new(),
            inline_keys: HashMap::new(),
            next_id: 0,
            status: tr!(cx, "就绪"),
            status_bar,
            readout_seen: None,
            terminal,
            custom_color_inputs,
            session_prefs,
            forward_prefs,
            macros,
            macro_manager: None,
            snippets,
            snippet_manager: None,
            history,
            history_seen: HashMap::new(),
            history_manager: None,
            split_state,
            view_split: None,
            focus_handle: cx.focus_handle(),
        };
        // 镜像分发循环：只在开关打开时把字节写进其它会话。实体销毁后 update 失败即退出。
        cx.spawn(async move |this, cx| {
            while let Some((source, origin, bytes)) = mirror_rx.recv().await {
                if this
                    .update(cx, |this, cx| this.mirror_input(source, origin, bytes, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // 键宏触发键：在所有动作分发之前拦下（否则 Ctrl+字母会先被终端当控制字符）。
        // 只在活动终端持有焦点且无浮层时生效，其余情况原样放行。
        let this = cx.entity().downgrade();
        cx.intercept_keystrokes(move |event, window, cx| {
            if let Some(entity) = this.upgrade() {
                entity.update(cx, |this, cx| this.intercept_macro_key(event, window, cx));
            }
        })
        .detach();
        app.watch_readout(cx);
        app.boot_vault(window, cx);
        // 启动档 shell 只有几个，图标走 shell 缓存、毫秒级，直接在主线程取好再进列表。
        app.refresh_shell_icons();
        app.auto_scan_local_shells(cx);
        // 桌面标签捕获键盘时，组合键必须先于全局绑定交给远端：否则 Ctrl+W / Ctrl+B
        // 这类应用快捷键会在捕获期间把标签关掉或收起边栏。拦截器只挡动作分发，
        // capture_key_down 仍会照常把按键送给会话。
        let entity = cx.entity().downgrade();
        cx.intercept_keystrokes(move |_, window, cx| {
            if let Some(this) = entity.upgrade()
                && this.read(cx).desktop_capturing(window)
            {
                cx.stop_propagation();
            }
        })
        .detach();
        app
    }

    /// 把命令行快速连接的目标登记进主机列表（同 user@host:port 已存在则跳过）。
    ///
    /// 只在加密库尚未打开时能确定 id，这里先留 0：`attach_vault` 会为所有
    /// id==0 的主机补发主键并入库；降级模式则直接落在旧 JSON 里。
    pub(super) fn adopt_quick_host(&mut self, target: &SshTarget, cx: &mut Context<Self>) {
        let exists = self.hosts.iter().any(|host| {
            host.host == target.host && host.port == target.port && host.user == target.user
        });
        if exists {
            return;
        }
        // 密码不落盘：认证方式只决定下次连接时向用户要什么。
        let auth = if target.key_path.is_some() || target.key_data.is_some() {
            AuthMethod::IdentityFile
        } else if target.password.is_some() {
            AuthMethod::Password
        } else {
            AuthMethod::Agent
        };
        let name = if target.port == 22 {
            target.host.clone()
        } else {
            format!("{}:{}", target.host, target.port)
        };
        let host = HostConfig {
            protocol: ProtocolKind::Ssh,
            id: 0,
            name,
            host: target.host.clone(),
            port: target.port,
            user: target.user.clone(),
            auth,
            key_path: target
                .key_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            key_inline: false,
            favorite: false,
            proxy_jump: None,
        };
        self.hosts.push(host.clone());
        self.persist_host(&host, cx);
        self.status = SharedString::from(format!("已把 {} 加入主机列表", host.addr()));
        cx.notify();
    }

    /// 异步打开加密凭据库：成功则接管主机列表（含一次性迁移旧库/JSON 并改名保留），
    /// 失败则降级回旧 JSON 存储并提示（降级模式下密码只留在内存）。
    ///
    /// 口令档（已设置主密码）启动时弹出解锁对话框，口令错误可重试；
    /// keyring 档保持零输入直连。
    pub(super) fn boot_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_opening || self.vault_maintenance { return; }
        let mode = self.mode.clone();
        if let Err(error) = crate::vault::recover_rekey(&mode) {
            self.status = format!("凭据库事务恢复失败，已禁止连接：{error:#}").into();
            cx.notify();
            return;
        }
        let generation = self.auth_gate.generation();
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
        if passphrase_needed { return; }
        self.vault_opening = true;
        cx.spawn(async move |this, cx| {
            // 库文件在数据目录还是原样，先探测主密钥是否可得。
            let keyring_key = match crate::vault::load_or_create_master_key() {
                Ok(key) => Some(key),
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.vault_opening = false;
                        if this.auth_gate.generation() != generation { return; }
                        log::warn!("系统凭据管理器不可用: {error}");
                        this.status =
                            SharedString::from(format!("凭据管理器不可用（{error}），SSH 连接已禁用"));
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
                        this.vault_opening = false;
                        if this.auth_gate.generation() != generation { return; }
                        this.attach_vault(vault, hosts, Some(key), cx);
                    });
                }
                Err(error) => {
                    log::warn!("加密凭据库不可用: {error}");
                    let _ = this.update(cx, |this, cx| {
                        this.vault_opening = false;
                        if this.auth_gate.generation() != generation { return; }
                        if crate::vault::read_unlock_marker(&this.mode)
                            == crate::vault::UnlockBackend::Passphrase
                        {
                            // 口令档打不开：交回解锁弹窗处理，不降级 JSON。
                            cx.notify();
                        } else {
                            this.status = SharedString::from(format!(
                                "加密凭据库不可用（{error}），SSH 连接已禁用"
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
    pub(super) fn attach_vault(
        &mut self,
        vault: Vault,
        mut db_hosts: Vec<HostConfig>,
        key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let vault = Arc::new(vault);
        let generation = self.auth_gate.generation();
        self.unlock = None;
        // 旧版本快速连接登记曾把同一台主机重复入库：字段完全一致时只留最早一条，
        // 其余从库里删掉（名字、端口等有差异的条目不受影响）。
        let mut unique: Vec<HostConfig> = Vec::with_capacity(db_hosts.len());
        let mut stale: Vec<i64> = Vec::new();
        for host in db_hosts.into_iter() {
            if unique.iter().any(|kept| same_saved_host(kept, &host)) {
                if host.id != 0 {
                    stale.push(host.id);
                }
                continue;
            }
            unique.push(host);
        }
        db_hosts = unique;
        for id in stale {
            let vault = vault.clone();
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.remove(id).await {
                    log::warn!("清理重复主机失败: {error}");
                }
            })
            .detach();
        }
        // 开库完成前用户新增的主机（id==0，彼时尚无法入库）：补发 id 并入库。
        let mut next = db_hosts.iter().map(|host| host.id).max().unwrap_or(0) + 1;
        for host in self.hosts.iter().filter(|host| host.id == 0) {
            // 快速连接登记时库还没打开，无法比对；库里已有同一 user@host:port 就跳过。
            let duplicate = db_hosts.iter().any(|saved| {
                saved.host == host.host && saved.port == host.port && saved.user == host.user
            });
            if duplicate {
                continue;
            }
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
        // 私钥与连接偏好加载完毕之后才打开认证门闩。
        cx.spawn(async move |this, cx| {
            let keys = match vault.load_host_keys().await {
                Ok(keys) => keys,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        if this.auth_gate.generation() == generation {
                            this.status = format!("加载私钥失败，SSH 连接已禁用：{error}").into();
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            if let Ok(Some(theme)) = vault.setting("theme").await {
                let dark = theme != "light";
                let _ = this.update(cx, |this, cx| {
                    this.store.save_theme_mirror(dark);
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
            // 库内语言优先于启动镜像（换机器、或镜像写入前进程被杀时对不上）。
            if let Ok(Some(locale)) = vault.setting("language").await
                && !locale.is_empty()
            {
                let locale = locale.clone();
                let _ = this.update(cx, |this, cx| {
                    if crate::i18n::current_locale(cx) != locale.as_str() {
                        this.apply_language(&locale, cx);
                    }
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
            // 托盘开关：启动时读 settings.json；库内若有更新值（换机器等）在此补齐。
            if let Ok(Some(value)) = vault.setting("tray").await {
                let enabled = value == "1" || value.eq_ignore_ascii_case("true");
                let _ = this.update(cx, |this, cx| this.set_tray_enabled(enabled, cx));
            }
            // 侧边栏分组同样以库内为准。
            if let Ok(Some(json)) = vault.setting("sidebar").await {
                let prefs = SidebarPrefs::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.sidebar != prefs {
                        let side = prefs.side;
                        this.sidebar = prefs;
                        this.split_state.update(cx, |state, cx| {
                            state.set_ratio(default_sidebar_ratio(side), cx);
                        });
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
            // 协议映射同样以库内为准；库里读出的主机一律按 SSH 看待，
            // 因此还要把开库前新增 / 迁移进来的 Telnet 选择并进偏好，避免被覆盖。
            let stored_protocols = vault
                .setting("protocols")
                .await
                .ok()
                .flatten()
                .map(|json| ProtocolPrefs::from_json(&json))
                .unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                let mut prefs = stored_protocols;
                let mut changed = false;
                for host in &this.hosts {
                    if host.id != 0 && host.protocol != ProtocolKind::Ssh {
                        changed |= prefs.set(host.id, host.protocol);
                    }
                }
                if changed {
                    this.persist_protocols(cx);
                }
                if this.protocols != prefs {
                    this.protocols = prefs;
                    this.apply_protocols();
                    cx.notify();
                }
            });
            // 本机文件页偏好同样以库内为准：还没列过目录时才回填上次停留的目录，
            // 别把用户已经翻进去的目录覆盖掉。
            if let Ok(Some(json)) = vault.setting("local_files").await {
                let prefs = LocalFilesPrefs::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if !this.local.listed && this.local.path.is_none() {
                        this.local.path = prefs.path.as_deref().map(std::path::PathBuf::from);
                    }
                    if this.local.show_hidden != prefs.show_hidden {
                        this.local.show_hidden = prefs.show_hidden;
                        cx.notify();
                    }
                });
            }
            // 键宏同样以库内为准（触发键每次按键才解析，无需额外回填）。
            if let Ok(Some(json)) = vault.setting("macros").await {
                let macros = crate::macros::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.auth_gate.generation() != generation { return; }
                    if this.macros != macros {
                        this.macros = macros;
                        cx.notify();
                    }
                });
            }
            // 命令片段同样以库内为准。
            if let Ok(Some(json)) = vault.setting("snippets").await {
                let snippets = crate::snippets::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.auth_gate.generation() != generation { return; }
                    if this.snippets != snippets {
                        this.snippets = snippets;
                        cx.notify();
                    }
                });
            }
            // 命令历史同样以库内为准。
            if let Ok(Some(json)) = vault.setting("history").await {
                let history = CommandHistory::from_json(&json);
                let _ = this.update(cx, |this, cx| {
                    if this.auth_gate.generation() != generation { return; }
                    if this.history != history {
                        this.history = history;
                        cx.notify();
                    }
                });
            }
            drop(vault);
            let Some(entity) = this.upgrade() else { return; };
            let _ = cx.with_window(entity.entity_id(), move |window, cx| {
                entity.update(cx, |this, cx| {
                    if this.auth_gate.generation() != generation || this.vault.is_none() { return; }
                    this.inline_keys = keys.into_iter().collect();
                    this.auth_gate.attach();
                    if let Some(target) = this.auth_gate.take_pending(&mut this.pending_quick) {
                        this.adopt_quick_host(&target, cx);
                        this.connect(target.addr(), target, None, None, false, window, cx);
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 当前主题对应的调色板（从全局 Theme 语义色组装）。
    pub(super) fn palette(&self, cx: &App) -> Palette {
        Palette::from_theme(cx)
    }

    /// 是否有浮层占着键盘（弹窗 / 表单 / 待输密码面板）。
    pub(super) fn overlay_open(&self) -> bool {
        self.unlock.is_some()
            || self.settings.is_some()
            || self.confirm_delete.is_some()
            || self.new_host.is_some()
            || self.group_prompt.is_some()
            || self.shell_prompt.is_some()
            || self.pending.is_some()
            || self.macro_manager.is_some()
            || self.snippet_manager.is_some()
            || self.history_manager.is_some()
            || self.show_discovery
    }

    /// 窗口级浮层：删除二次确认、主机表单、分组新建/重命名等居中模态。
    /// （主机右键菜单不在这里——它走框架的独立图层，见 `context_menu`。）
    pub(super) fn render_overlay(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        // 解锁 / 主密码弹窗优先于一切浮层（库未解锁时其余操作无意义）。
        if self.unlock.is_some() {
            return self.render_unlock_dialog(cx);
        }
        if self.settings.is_some() {
            return self.render_settings_dialog(cx);
        }
        if self.macro_manager.is_some() {
            return self.render_macro_manager(cx);
        }
        if self.snippet_manager.is_some() {
            return self.render_snippet_manager(cx);
        }
        if self.history_manager.is_some() {
            return self.render_history_manager(cx);
        }
        if self.file_prompt.is_some() {
            return self.render_file_prompt(cx);
        }
        if self.file_confirm.is_some() {
            return self.render_file_confirm(cx);
        }
        if self.file_upload_prompt.is_some() {
            return self.render_file_upload_prompt(cx);
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
            return self.render_form_dialog(window, cx);
        }
        // 局域网发现：小工具弹窗，列表只在需要时出现，不占主机列表。
        if self.show_discovery {
            return self.render_discovery_dialog(cx);
        }
        // 分组新建 / 重命名：同名模态，宽度固定，避免列表内联编辑误丢内容。
        if self.group_prompt.is_some() {
            return self.render_group_prompt(cx);
        }
        if self.shell_prompt.is_some() {
            return self.render_shell_prompt(cx);
        }
        if self.broadcast.is_some() {
            return self.render_broadcast_dialog(cx);
        }
        div().into_any_element()
    }

    pub(super) fn render_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                        .children(self.tabs.iter().enumerate().map(|(index, tab)| {
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
                                // 拖拽排序：拖动超过阈值后出现跟随光标的缩略标签，
                                // 松手落到哪个标签上就占它的位置（空白处＝移到最末）。
                                .on_drag(
                                    TabDragData {
                                        index,
                                        title: tab.title.to_string(),
                                        position: Point::default(),
                                    },
                                    |drag: &TabDragData, position, _window, cx| {
                                        cx.new(|_| TabDragData {
                                            index: drag.index,
                                            title: drag.title.clone(),
                                            position,
                                        })
                                    },
                                )
                                .drag_over::<TabDragData>(move |style, _, _, _| {
                                    style
                                        .bg(pal.card_hover)
                                        .border_l(px(2.0))
                                        .border_color(pal.accent)
                                })
                                .on_drop(cx.listener(move |this, drag: &TabDragData, _, cx| {
                                    this.reorder_tab(drag.index, index, cx);
                                }))
                                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                    // 双击就地改名，单击仍是切标签（改名时也别丢激活态）。
                                    if event.click_count() >= 2 {
                                        this.begin_rename(id, window, cx);
                                    } else {
                                        this.activate_tab(id, window, cx);
                                    }
                                    cx.notify();
                                }))
                                // 中键关标签：浏览器/终端用户的肌肉记忆，省一次「先切过去再点 ×」。
                                .on_mouse_down(
                                    MouseButton::Middle,
                                    cx.listener(move |this, _, _, cx| this.close_tab(id, cx)),
                                )
                                .tooltip({
                                    let tip = tr!(cx, "单击切换 · 双击重命名 · 中键关闭");
                                    move |window, cx| Tooltip::new(tip.clone()).build(window, cx)
                                })
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
                                // 远程桌面标签与终端标签同栏排列，靠图标区分类型。
                                .when(tab.desktop.is_some(), |this| {
                                    this.child(
                                        Icon::new(IconName::Frame)
                                            .small()
                                            .flex_shrink_0()
                                            .text_color(pal.muted),
                                    )
                                })
                                .child(match self.renaming.as_ref() {
                                    // 就地改名：标题位置换成输入框，回车/失焦提交，Esc 取消。
                                    // 宽度要给死值：标签栏是横向滚动容器，主轴空间无限，
                                    // `flex_1` 的输入框会算成 0 宽，文字全被裁掉。
                                    Some((renaming_id, state)) if *renaming_id == id => div()
                                        .key_context("TabRename")
                                        .w(px(180.0))
                                        .flex_shrink_0()
                                        .child(styled_input(Input::new(state).text_xs(), &pal)),
                                    _ => div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .child(tab.title.clone()),
                                })
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
                        }))
                        // 拖到标签栏空白处＝挪到最末，避免只能落到某个标签上。
                        .on_drop(cx.listener(|this, drag: &TabDragData, _, cx| {
                            let last = this.tabs.len().saturating_sub(1);
                            this.reorder_tab(drag.index, last, cx);
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
                .unwrap_or_else(|| self.render_empty_state(&pal, cx))
        };

        v_flex().flex_1().h_full().child(tab_bar).child(body)
    }

}

impl Render for RuTermApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 换下来的远程桌面旧帧在这里释放：精灵图集按图像 id 缓存，不释放会越积越多。
        self.drop_desktop_images(window, cx);
        let pal = self.palette(cx);
        v_flex()
            .id("ruterm-root")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(pal.window_bg)
            .text_color(pal.text)
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_file_panel_action))
            .on_action(cx.listener(Self::toggle_local_panel))
            .on_action(cx.listener(Self::toggle_theme_action))
            .on_action(cx.listener(Self::lock_vault_action))
            .on_action(cx.listener(Self::new_host))
            .on_action(cx.listener(Self::new_local_terminal))
            .on_action(cx.listener(Self::new_desktop))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::close_active_tab))
            .on_action(cx.listener(Self::cancel_rename))
            .on_action(cx.listener(Self::open_settings_action))
            .on_action(cx.listener(Self::open_forward_action))
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::split_down))
            .on_action(cx.listener(Self::close_split_action))
            .on_action(cx.listener(Self::find_in_terminal))
            .on_action(cx.listener(Self::close_find))
            .on_action(cx.listener(Self::toggle_sync_input))
            .on_action(cx.listener(Self::broadcast_command))
            .on_action(cx.listener(Self::cancel_broadcast))
            .on_action(cx.listener(Self::open_macro_manager))
            .on_action(cx.listener(Self::close_macro_manager_action))
            .on_action(cx.listener(Self::open_snippet_manager))
            .on_action(cx.listener(Self::close_snippet_manager_action))
            .on_action(cx.listener(Self::open_history_manager))
            .on_action(cx.listener(Self::close_history_manager_action))
            .on_action(cx.listener(Self::terminal_copy_action))
            .on_action(cx.listener(Self::terminal_copy_text_action))
            .on_action(cx.listener(Self::terminal_paste_action))
            // 捕获阶段先看到回车：断开的终端已写不到远端，回车要用来重连而不是被它吞掉。
            .capture_key_down(cx.listener(Self::capture_session_key))
            .child(self.render_title_bar(window, cx))
            .child({
                // 边栏在哪一侧由设置决定：图标栏恒贴在窗口外缘，面板在它内侧，
                // 中间是可拖拽的分隔条；面板收起时图标栏仍然留着（RustRover 的工具窗口栏）。
                // 这里用普通 flex 行而不是 SplitPane：SplitPane 的面板是块级容器，
                // Taffy 量它的 min-content 时会把面板内的子元素按内容宽写死，
                // 面板自身的最终布局命中缓存后就不再纠正（终端被挤成标签栏那么宽）。
                let visible = self.sidebar_visible(cx);
                let panel_ratio = self.panel_ratio(cx);
                let width_sink = self.split_width.clone();
                let row = h_flex()
                    .relative()
                    .size_full()
                    .min_w_0()
                    .child(
                        // 回填容器宽度给拖拽换算；绝对定位不参与布局。
                        canvas(
                            move |bounds, _, _| width_sink.set(bounds.size.width.as_f32()),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(self.render_sidebar_rail(cx));
                // 每个 flex 项都显式 min_w_0：否则 Taffy 会按 min-content 量它，
                // 量出来的内容宽会被写进子树布局（见上）。
                let main = div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .child(self.render_main(cx))
                    .into_any_element();
                let panel = visible.then(|| {
                    div()
                        .w(relative(panel_ratio))
                        .flex_shrink_0()
                        .min_w_0()
                        .h_full()
                        .overflow_hidden()
                        .child(self.render_sidebar_panel(cx))
                        .into_any_element()
                });
                match self.sidebar.side {
                    SidebarSide::Left => row
                        .when_some(panel, |row, panel| {
                            row.child(panel).child(self.render_split_divider(cx))
                        })
                        .child(main)
                        .into_any_element(),
                    SidebarSide::Right => row
                        .child(main)
                        .when_some(panel, |row, panel| {
                            row.child(self.render_split_divider(cx)).child(panel)
                        })
                        .child(self.render_sidebar_rail(cx))
                        .into_any_element(),
                }
            })
            .child(self.render_status_bar(cx).flex_shrink_0().w_full())
            .child(self.render_overlay(window, cx))
            .child(self.render_terminal_menu())
    }
}

impl Focusable for RuTermApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

