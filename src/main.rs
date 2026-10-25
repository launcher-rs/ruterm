//! RuLink：基于 rgpui + rgpui-term 外接 IO 后端的 SSH 客户端。
//!
//! 形态为 GUI 会话管理器：无参数启动即进入主界面，左侧是持久化的主机账号列表，
//! 点击账号即连接；密码认证的账号在连接时临时输入密码（绝不落盘），私钥/Agent
//! 认证直接连接。仍兼容一次性命令行快速连接（带目标参数时首启动即连上该主机）。

mod config;
mod session;
mod sessions;
mod ssh;

use std::collections::HashMap;

use rgpui::components::status_bar::{StatusBar, StatusBarItem};
use rgpui::input_ui::{Input, InputContentType, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, App, Bounds, Button, ButtonVariants, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, ScrollableElement,
    SharedString, StatefulInteractiveElement, Styled, TrayMenuItem, Window, WindowBounds,
    WindowOptions, div, h_flex, px, rgb, size, v_flex,
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
    /// 远程服务器信息（预留扩展位，本次恒为 None；将来由 SSH 查询任务填充后在状态栏展示）。
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
    /// 侧边栏是否展开（折叠后收成窄轨道，仅留一个展开按钮）。
    sidebar_open: bool,
    /// 是否为暗色主题（true 暗色，false 亮色）。
    dark: bool,
    focus_handle: FocusHandle,
}

/// 主题调色板：一次渲染内的所有界面颜色都从这里取（存原始十六进制，用 `rgb` 现取）。
/// 切换明暗只改 `RuLinkApp::dark`，界面整体随之换色。
struct Palette {
    /// 窗口/编辑区底色。
    window_bg: u32,
    /// 侧边栏与标签栏底色。
    sidebar_bg: u32,
    /// 卡片/表单/非活动标签底色。
    card_bg: u32,
    /// 卡片悬停底色。
    card_hover: u32,
    /// 分隔边框色。
    border: u32,
    /// 主文字色。
    text: u32,
    /// 次要/弱化文字色。
    muted: u32,
    /// 认证方式链接色。
    link: u32,
    /// 删除/关闭等危险操作色。
    danger: u32,
    /// 选中/主按钮强调色。
    accent: u32,
    /// 未选中分段（认证方式切换）底色。
    segment_bg: u32,
    /// 输入框底色。
    input_bg: u32,
    /// 输入框文字色。
    input_text: u32,
    /// 输入框边框色。
    input_border: u32,
}

impl Palette {
    /// 暗色配色（沿用初始手工调好的 VSCode 深色值）。
    fn dark() -> Self {
        Palette {
            window_bg: 0x1e1e1e,
            sidebar_bg: 0x252526,
            card_bg: 0x2d2d30,
            card_hover: 0x37373d,
            border: 0x3a3a3a,
            text: 0xd6d6d6,
            muted: 0x8a8a8a,
            link: 0x6fa8dc,
            danger: 0xbf5b5b,
            accent: 0x0e639c,
            segment_bg: 0x3a3a3a,
            input_bg: 0x1b1b1c,
            input_text: 0xf2f2f2,
            input_border: 0x4a4a4a,
        }
    }

    /// 亮色配色。
    fn light() -> Self {
        Palette {
            window_bg: 0xf5f5f5,
            sidebar_bg: 0xececec,
            card_bg: 0xffffff,
            card_hover: 0xe0e0e0,
            border: 0xcfcfcf,
            text: 0x2a2a2a,
            muted: 0x767676,
            link: 0x0e639c,
            danger: 0xc0392b,
            accent: 0x0e639c,
            segment_bg: 0xdcdcdc,
            input_bg: 0xffffff,
            input_text: 0x1e1e1e,
            input_border: 0xb8b8b8,
        }
    }
}

impl RuLinkApp {
    /// 载入已保存主机，可选执行命令行快速连接。
    fn new(quick: Option<SshTarget>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = SessionStore::new();
        let hosts = store.load();
        let mut app = Self {
            store,
            hosts,
            tabs: Vec::new(),
            active_tab: None,
            new_host: None,
            pending: None,
            password_cache: HashMap::new(),
            next_id: 0,
            status: "就绪".into(),
            sidebar_open: true,
            dark: true,
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
        let session = session::start(target, handles, cx);
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
            let result = session.await;
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

    /// 当前主题对应的调色板。
    fn palette(&self) -> Palette {
        if self.dark {
            Palette::dark()
        } else {
            Palette::light()
        }
    }

    /// 切换明暗主题：翻转本地配色并同步 rgpui 全局主题（状态栏/按钮随之换色）。
    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dark = !self.dark;
        let mode = if self.dark {
            rgpui::theme::ThemeMode::Dark
        } else {
            rgpui::theme::ThemeMode::Light
        };
        rgpui::theme::Theme::change(mode, Some(window), cx);
        cx.notify();
    }
}

impl Render for RuLinkApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = self.palette();
        v_flex()
            .id("rulink-root")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(pal.window_bg))
            .text_color(rgb(pal.text))
            .on_key_down(
                cx.listener(|this, event: &rgpui::KeyDownEvent, window, cx| {
                    // Ctrl+B 折叠 / 展开侧边栏。
                    if event.keystroke.key == "b" && event.keystroke.modifiers.control {
                        this.sidebar_open = !this.sidebar_open;
                        cx.notify();
                    }
                    // Ctrl+T 切换明暗主题。
                    if event.keystroke.key == "t" && event.keystroke.modifiers.control {
                        this.toggle_theme(window, cx);
                    }
                }),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.render_sidebar(cx))
                    .child(self.render_main(cx)),
            )
            .child(self.render_status_bar().flex_shrink_0().w_full())
    }
}

impl RuLinkApp {
    /// 左侧栏：展开时显示标题 / 新建按钮 / 主机列表面板；折叠时收成窄轨道，仅留展开按钮。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette();
        if !self.sidebar_open {
            return v_flex()
                .w(px(44.0))
                .h_full()
                .flex_shrink_0()
                .items_center()
                .pt_2()
                .bg(rgb(pal.sidebar_bg))
                .border_r_1()
                .border_color(rgb(pal.border))
                .child(
                    Button::new("sidebar-expand")
                        .label("»")
                        .tooltip("展开侧边栏 (Ctrl+B)")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_open = true;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }

        let panel = if self.new_host.is_some() {
            self.render_form_panel(cx)
        } else if self.pending.is_some() {
            self.render_pending_panel(cx)
        } else {
            self.render_host_list(cx)
        };

        let theme_label: &'static str = if self.dark { "☀" } else { "☾" };
        let theme_tooltip: &'static str = if self.dark {
            "切换到亮色主题 (Ctrl+T)"
        } else {
            "切换到暗色主题 (Ctrl+T)"
        };

        v_flex()
            .w(px(280.0))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(pal.sidebar_bg))
            .border_r_1()
            .border_color(rgb(pal.border))
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
                                    .tooltip("折叠侧边栏 (Ctrl+B)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sidebar_open = false;
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
            .child(div().flex_1().overflow_y_scrollbar().child(panel))
            .into_any_element()
    }

    /// 主机账号列表；每行可点击连接、附删除按钮。
    fn render_host_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette();
        if self.hosts.is_empty() {
            return div()
                .text_sm()
                .text_color(rgb(pal.muted))
                .child("暂无主机，点击上方“新建主机”添加。")
                .into_any_element();
        }
        v_flex()
            .gap_1()
            .children(self.hosts.iter().enumerate().map(|(index, host)| {
                let host_id = SharedString::from(format!("host-row-{index}"));
                let del_id = SharedString::from(format!("host-del-{index}"));
                h_flex()
                    .id(host_id)
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .rounded(px(6.0))
                    .bg(rgb(pal.card_bg))
                    .hover(|style| style.bg(rgb(pal.card_hover)))
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.click_host(index, window, cx)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .child(
                                div()
                                    .font_weight(rgpui::FontWeight(600.0))
                                    .child(host.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(pal.muted))
                                    .child(host.addr()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(pal.link))
                                    .child(auth_label(host.auth)),
                            )
                            .child(
                                div()
                                    .id(del_id)
                                    .text_sm()
                                    .text_color(rgb(pal.danger))
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
        let pal = self.palette();
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
            .bg(rgb(pal.card_bg))
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
                                rgb(pal.accent)
                            } else {
                                rgb(pal.segment_bg)
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
        let pal = self.palette();
        let Some(pending) = self.pending.as_ref() else {
            return div().into_any_element();
        };
        let host = pending.host.addr();
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(rgb(pal.card_bg))
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
        let pal = self.palette();
        let tab_bar = if self.tabs.is_empty() {
            div()
        } else {
            h_flex()
                .h(px(36.0))
                .flex_shrink_0()
                .items_center()
                .bg(rgb(pal.sidebar_bg))
                .border_b_1()
                .border_color(rgb(pal.border))
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
                        .bg(if active {
                            rgb(pal.window_bg)
                        } else {
                            rgb(pal.card_bg)
                        })
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
                                .text_color(rgb(pal.danger))
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
                            .text_color(rgb(pal.muted))
                            .child("从左侧点击主机账号开始连接"),
                    )
                    .into_any_element()
            });

        v_flex().flex_1().h_full().child(tab_bar).child(body)
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
            // 远程服务器信息扩展位：将来由 SSH 查询任务写入 tab.remote_info 后自动出现。
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
        .bg(rgb(pal.input_bg))
        .text_color(rgb(pal.input_text))
        .border_1()
        .border_color(rgb(pal.input_border))
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
        // theme::init 默认切到亮色，输入框背景近白；本程序整体是暗色配色，
        // 文字继承浅灰会洗白看不清，这里显式切到暗色主题统一底色。
        rgpui::theme::Theme::change(rgpui::theme::ThemeMode::Dark, None, cx);
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
                cx.new(|cx| RuLinkApp::new(quick, window, cx))
            },
        );
        if opened.is_err() {
            cx.quit();
            return;
        }
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
        cx.on_tray_menu_action(|id: SharedString, cx: &mut App| match id.as_ref() {
            "quit" => cx.quit(),
            "show" => cx.activate(true),
            _ => {}
        });
    });
}
