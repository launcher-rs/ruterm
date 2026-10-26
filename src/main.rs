//! RuLink：基于 rgpui + rgpui-term 外接 IO 后端的 SSH 客户端。
//!
//! 形态为 GUI 会话管理器：无参数启动即进入主界面，左侧是持久化的主机账号列表，
//! 点击账号即连接；密码认证的账号在连接时临时输入密码（绝不落盘），私钥/Agent
//! 认证直接连接。仍兼容一次性命令行快速连接（带目标参数时首启动即连上该主机）。

mod config;
mod remote;
mod session;
mod sessions;
mod ssh;

use std::collections::HashMap;

use rgpui::components::split_pane::{CollapsiblePane, SplitPane, SplitPaneState};
use rgpui::components::status_bar::{StatusBar, StatusBarItem};
use rgpui::input_ui::{Input, InputContentType, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, App, Bounds, Button, ButtonVariants, Context, Entity, FocusHandle, Focusable, Hsla,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, ScrollableElement,
    SharedString, StatefulInteractiveElement, Styled, TrayMenuItem, Window, WindowBounds,
    WindowOptions, div, h_flex, px, size, v_flex,
};
use rgpui_term::{TerminalBuilder, TerminalView};

use crate::config::SshTarget;
use crate::sessions::{AuthMethod, HostConfig, SessionStore};

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
    /// 连接位置（user@host:port），状态栏展示；将来远程指标也挂在这里。
    addr: SharedString,
    /// 认证方式，状态栏展示。
    auth: AuthMethod,
    /// 远程服务器基本信息摘要（连接后经 exec 探测异步填充，探测失败保持 None）。
    remote_info: Option<SharedString>,
    /// 渲染用终端视图。
    terminal_view: Entity<TerminalView>,
}

/// 新建主机表单（各字段为受控输入状态）。
struct NewHostForm {
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    key_path: Entity<InputState>,
    auth: AuthMethod,
}

/// 待输入密码后连接的会话（点击密码认证主机时进入）。
struct PendingConnect {
    host: HostConfig,
    password: Entity<InputState>,
}

/// RuLink 主视图：左侧主机账号管理，右侧会话标签终端。
struct RuLinkApp {
    /// 会话仓库（负责持久化）。
    store: SessionStore,
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
    /// 本次运行内缓存的密码（按 user@host:port），避免重复弹窗；绝不落盘。
    password_cache: HashMap<String, String>,
    /// 标签自增 id。
    next_id: usize,
    /// 状态栏左侧的瞬态消息（连接/保存/删除/断开等节点更新）。
    status: SharedString,
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
        }
    }
}

impl RuLinkApp {
    /// 载入已保存主机，可选执行命令行快速连接。
    fn new(quick: Option<SshTarget>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = SessionStore::new();
        let hosts = store.load();
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
            hosts,
            search,
            tabs: Vec::new(),
            active_tab: None,
            new_host: None,
            pending: None,
            password_cache: HashMap::new(),
            next_id: 0,
            status: "就绪".into(),
            split_state,
            focus_handle: cx.focus_handle(),
        };
        if let Some(target) = quick {
            let title = target.addr();
            app.connect(title, target, None, window, cx);
        }
        app
    }

    /// 打开新建主机表单，填充默认端口。
    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let default_user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "root".to_string());
        self.new_host = Some(NewHostForm {
            name: cx.new(|cx| InputState::new(window, cx).placeholder("展示名（留空用主机地址）")),
            host: cx.new(|cx| InputState::new(window, cx).placeholder("host 或 IP")),
            port: cx.new(|cx| InputState::new(window, cx).default_value("22")),
            user: cx.new(|cx| InputState::new(window, cx).default_value(default_user)),
            key_path: cx.new(|cx| InputState::new(window, cx).placeholder("~/.ssh/id_ed25519")),
            auth: AuthMethod::Agent,
        });
        // 表单打开即把焦点交给"主机地址"输入框：退格/删除等键位动作只在输入框
        // 持有焦点（"Input" 键位上下文激活）时才会派发，主动聚焦让删除键即时可用。
        if let Some(form) = self.new_host.as_ref() {
            form.host.update(cx, |state, cx| state.focus(window, cx));
        }
        self.status = "填写主机信息".into();
        cx.notify();
    }

    /// 关闭新建表单（不保存）。
    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        self.new_host = None;
        self.status = "就绪".into();
        cx.notify();
    }

    /// 保存新建表单到列表并持久化；校验失败则保留表单。
    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.new_host.as_ref() else {
            return;
        };
        let host_raw = form.host.read(cx).value().trim().to_string();
        let host = HostConfig {
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
            auth: form.auth,
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
        self.hosts.push(host);
        self.store.save(&self.hosts);
        self.new_host = None;
        self.status = SharedString::from(format!("已保存主机 {label}"));
        cx.notify();
    }

    /// 删除指定主机并持久化。
    fn delete_host(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.hosts.len() {
            let label = self.hosts[index].name.clone();
            self.hosts.remove(index);
            self.store.save(&self.hosts);
            self.status = SharedString::from(format!("已删除主机 {label}"));
            cx.notify();
        }
    }

    /// 切换指定主机的收藏状态并持久化。
    fn toggle_favorite(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(host) = self.hosts.get_mut(index) {
            host.favorite = !host.favorite;
            self.store.save(&self.hosts);
            cx.notify();
        }
    }

    /// 点击主机账号：密码认证优先用缓存，无缓存才弹窗；其余直接连接。
    fn click_host(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get(index).cloned() else {
            return;
        };
        if host.auth == AuthMethod::Password {
            let key = host.addr();
            if let Some(password) = self.password_cache.get(&key).cloned() {
                let target = SshTarget::from_host(&host, Some(password));
                self.connect(host.name.clone(), target, Some(key), window, cx);
                return;
            }
            let password =
                cx.new(|cx| InputState::new(window, cx).placeholder("输入密码后点击连接"));
            password.update(cx, |state, cx| state.focus(window, cx));
            self.pending = Some(PendingConnect { host, password });
            cx.notify();
        } else {
            let target = SshTarget::from_host(&host, None);
            self.connect(host.name.clone(), target, None, window, cx);
        }
    }

    /// 取消待输入密码的连接。
    fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        cx.notify();
    }

    /// 用输入的密码完成待连接，并缓存到本次运行内存中。
    fn confirm_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let password = pending.password.read(cx).value().to_string();
        let host = pending.host.clone();
        self.pending = None;
        let key = host.addr();
        self.password_cache.insert(key.clone(), password.clone());
        let target = SshTarget::from_host(&host, Some(password));
        self.connect(host.name.clone(), target, Some(key), window, cx);
    }

    /// 建立外接终端并启动 SSH 会话，追加为新标签。
    ///
    /// 会话任务由后台 future 持有并等待：SSH 断开、远端 `exit`、认证失败都会
    /// 让任务结束，据此自动移除标签（对应"退出即关窗口"）；认证失败时顺带清掉
    /// 缓存密码，下次点击会重新询问。
    fn connect(
        &mut self,
        title: String,
        target: SshTarget,
        cache_key: Option<String>,
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
        let terminal_view = cx.new(|cx| TerminalView::new(terminal, window, cx));
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
        let session::SshSession { join, remote_info } = session::start(target, handles, cx);
        self.status = SharedString::from(format!("正在连接 {addr}…"));
        self.tabs.push(SessionTab {
            id,
            title: title.into(),
            addr: addr.clone().into(),
            auth,
            remote_info: None,
            terminal_view,
        });
        self.active_tab = Some(id);
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let _ = this.update(cx, |this, cx| {
                if result.is_err()
                    && let Some(key) = &cache_key
                {
                    this.password_cache.remove(key);
                }
                this.remove_tab(id, cx);
                if result.is_err() {
                    this.status = SharedString::from(format!("连接失败：{addr}"));
                    cx.notify();
                }
            });
        })
        .detach();
        // 远程基本信息比会话短得多：探测回来时标签可能已关闭，据此判断后再写入。
        cx.spawn(async move |this, cx| {
            if let Ok(info) = remote_info.await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == id) {
                        tab.remote_info = Some(SharedString::from(info));
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

    /// 切换明暗主题：翻转全局 Theme 模式并持久化，界面（含状态栏/按钮）随之重取配色。
    fn toggle_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = cx.theme().is_dark();
        let mode = if dark {
            rgpui::theme::ThemeMode::Light
        } else {
            rgpui::theme::ThemeMode::Dark
        };
        rgpui::theme::Theme::change(mode, Some(window), cx);
        self.store.save_theme(!dark);
        cx.notify();
    }
}

impl Render for RuLinkApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = self.palette(cx);
        v_flex()
            .id("rulink-root")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(pal.window_bg)
            .text_color(pal.text)
            .on_key_down(
                cx.listener(|this, event: &rgpui::KeyDownEvent, window, cx| {
                    // Ctrl+B 折叠 / 展开侧边栏（走分隔面板状态）。
                    if event.keystroke.key == "b" && event.keystroke.modifiers.control {
                        this.split_state.update(cx, |state, cx| {
                            state.toggle_collapse(CollapsiblePane::First, cx);
                        });
                        cx.notify();
                    }
                    // Ctrl+T 切换明暗主题。
                    if event.keystroke.key == "t" && event.keystroke.modifiers.control {
                        this.toggle_theme(window, cx);
                    }
                }),
            )
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
    }
}

impl RuLinkApp {
    /// 左侧栏：标题 + 主题切换 / 新建按钮 / 主机列表面板。
    /// 宽度与折叠由外层 SplitPane 负责，这里只填充第一面板区域。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let panel = if self.new_host.is_some() {
            self.render_form_panel(cx)
        } else if self.pending.is_some() {
            self.render_pending_panel(cx)
        } else {
            self.render_host_list(cx)
        };

        let dark = cx.theme().is_dark();
        let theme_label: &'static str = if dark { "☀" } else { "☾" };
        let theme_tooltip: &'static str = if dark {
            "切换到亮色主题 (Ctrl+T)"
        } else {
            "切换到暗色主题 (Ctrl+T)"
        };
        // 搜索框只在主机列表模式下出现（表单 / 待输入密码时隐藏）。
        let list_mode = self.new_host.is_none() && self.pending.is_none();

        v_flex()
            .size_full()
            .bg(pal.sidebar_bg)
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(rgpui::FontWeight(700.0))
                            .child("RuLink"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("toggle-theme")
                                    .label(theme_label)
                                    .ghost()
                                    .tooltip(theme_tooltip)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_theme(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("sidebar-collapse")
                                    .label("«")
                                    .ghost()
                                    .tooltip("折叠侧边栏 (Ctrl+B)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.split_state.update(cx, |state, cx| {
                                            state.collapse(CollapsiblePane::First, cx);
                                        });
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                Button::new("new-host")
                    .label("+ 新建主机")
                    .primary()
                    .w_full()
                    .on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))),
            )
            .when(list_mode, |this| {
                this.child(styled_input(Input::new(&self.search), &pal))
            })
            .child(div().flex_1().overflow_y_scrollbar().child(panel))
            .into_any_element()
    }

    /// 主机账号列表；每行可点击连接、附删除按钮。
    fn render_host_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        if self.hosts.is_empty() {
            return div()
                .text_sm()
                .text_color(pal.muted)
                .child("暂无主机，点击上方“新建主机”添加。")
                .into_any_element();
        }
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
        if matched.is_empty() {
            return div()
                .text_sm()
                .text_color(pal.muted)
                .child("无匹配主机")
                .into_any_element();
        }
        v_flex()
            .gap_1()
            .children(matched.into_iter().map(|index| {
                let host = &self.hosts[index];
                let host_id = SharedString::from(format!("host-row-{index}"));
                let del_id = SharedString::from(format!("host-del-{index}"));
                let fav_id = SharedString::from(format!("host-fav-{index}"));
                let fav_glyph: &'static str = if host.favorite { "★" } else { "☆" };
                h_flex()
                    .id(host_id)
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .rounded(px(6.0))
                    .bg(pal.card_bg)
                    .hover(|style| style.bg(pal.card_hover))
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.click_host(index, window, cx)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_ellipsis()
                                    .font_weight(rgpui::FontWeight(600.0))
                                    .child(host.name.clone()),
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
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .id(fav_id)
                                    .text_sm()
                                    .px_1()
                                    .text_color(if host.favorite { pal.accent } else { pal.muted })
                                    .child(fav_glyph)
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_favorite(index, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(pal.link)
                                    .child(auth_label(host.auth)),
                            )
                            .child(
                                div()
                                    .id(del_id)
                                    .text_sm()
                                    .text_color(pal.danger)
                                    .child("删除")
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.delete_host(index, cx)
                                    })),
                            ),
                    )
            }))
            .into_any_element()
    }

    /// 新建主机表单面板。
    fn render_form_panel(&self, cx: &mut Context<Self>) -> AnyElement {
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
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(pal.card_bg)
            .child(labeled_field("展示名", &form.name, &pal))
            .child(labeled_field("主机地址", &form.host, &pal))
            .child(labeled_field("端口", &form.port, &pal))
            .child(labeled_field("用户名", &form.user, &pal))
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child("认证方式"))
                    .child(h_flex().gap_2().children(methods.into_iter().map(|method| {
                        let auth_id = SharedString::from(format!("auth-{}", auth_label(method)));
                        div()
                            .id(auth_id)
                            .px_3()
                            .py_1()
                            .rounded(px(5.0))
                            .text_sm()
                            .cursor_pointer()
                            .bg(if method == selected {
                                pal.accent
                            } else {
                                pal.segment_bg
                            })
                            .child(auth_label(method))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = this.new_host.as_mut() {
                                    form.auth = method;
                                }
                                cx.notify();
                            }))
                    }))),
            )
            .when(selected == AuthMethod::IdentityFile, |this| {
                this.child(labeled_field("私钥路径", &form.key_path, &pal))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save-host")
                            .label("保存")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.save_form(cx))),
                    )
                    .child(
                        Button::new("cancel-host")
                            .label("取消")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
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
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(pal.card_bg)
            .child(
                div()
                    .text_sm()
                    .child(format!("连接 {host} 需要密码（仅本次使用，不保存）")),
            )
            .child(styled_input(
                Input::new(&pending.password).content_type(InputContentType::Password),
                &pal,
            ))
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

    /// 右侧主区：标签栏 + 当前终端。
    fn render_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = self.palette(cx);
        let tab_bar = if self.tabs.is_empty() {
            div()
        } else {
            h_flex()
                .h(px(36.0))
                .flex_shrink_0()
                .items_center()
                .bg(pal.sidebar_bg)
                .border_b_1()
                .border_color(pal.border)
                .px_2()
                .gap_1()
                .overflow_hidden()
                .children(self.tabs.iter().map(|tab| {
                    let id = tab.id;
                    let active = self.active_tab == Some(id);
                    let tab_id = SharedString::from(format!("tab-{id}"));
                    let close_id = SharedString::from(format!("tab-close-{id}"));
                    h_flex()
                        .id(tab_id)
                        .px_3()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .rounded(px(5.0))
                        .bg(if active { pal.window_bg } else { pal.card_bg })
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.active_tab = Some(id);
                            cx.notify();
                        }))
                        .child(div().child(tab.title.clone()))
                        .child(
                            div()
                                .id(close_id)
                                .text_sm()
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

        let body = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .map(|tab| {
                div()
                    .flex_1()
                    .overflow_hidden()
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
                .h(px(36.0))
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

    /// 底部状态栏：左侧瞬态状态 + 活动会话信息（地址/认证/远程指标占位），右侧会话计数。
    fn render_status_bar(&self) -> StatusBar {
        let active = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id));

        let mut left = vec![StatusBarItem::new(self.status.clone()).id("status")];
        if let Some(tab) = active {
            left.push(StatusBarItem::new(tab.addr.clone()).id("addr"));
            left.push(
                StatusBarItem::new(auth_label(tab.auth))
                    .id("auth")
                    .muted(true),
            );
            // 远程基本信息由 SSH 建立后的 exec 探测任务异步写入 tab.remote_info。
            if let Some(info) = tab.remote_info.clone() {
                left.push(StatusBarItem::new(info).id("remote").muted(true));
            }
        }
        let right = vec![
            StatusBarItem::new(format!("会话 {}", self.tabs.len()))
                .id("sessions")
                .muted(true),
        ];
        StatusBar::new().left(left).right(right)
    }
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
        // theme::init 默认切到亮色；这里按上次持久化的主题选择应用（缺省暗色），
        // 使输入框/界面底色与用户上次退出时的选择一致。
        let dark = SessionStore::new().load_settings().dark();
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
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.0), px(720.0)),
                    cx,
                ))),
                titlebar: Some(rgpui::TitlebarOptions {
                    title: Some("RuLink".into()),
                    ..Default::default()
                }),
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
