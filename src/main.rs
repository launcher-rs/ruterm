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

use rgpui::input_ui::{Input, InputContentType, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, App, Button, ButtonVariants, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, ScrollableElement,
    SharedString, StatefulInteractiveElement, Styled, Window, WindowOptions, div, h_flex, px, rgb,
    v_flex,
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
    focus_handle: FocusHandle,
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
        cx.notify();
    }

    /// 关闭新建表单（不保存）。
    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        self.new_host = None;
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
            return;
        }
        self.hosts.push(host);
        self.store.save(&self.hosts);
        self.new_host = None;
        cx.notify();
    }

    /// 删除指定主机并持久化。
    fn delete_host(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.hosts.len() {
            self.hosts.remove(index);
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
        let id = self.next_id;
        self.next_id += 1;
        let session = session::start(target, handles, cx);
        self.tabs.push(SessionTab {
            id,
            title: title.into(),
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
            });
        })
        .detach();
        cx.notify();
    }

    /// 用户点击标签关闭按钮。
    fn close_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        self.remove_tab(id, cx);
    }

    /// 移除标签并修正选中态。
    fn remove_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        let before = self.tabs.len();
        self.tabs.retain(|tab| tab.id != id);
        if self.tabs.len() == before {
            return;
        }
        if self.active_tab == Some(id) {
            self.active_tab = self.tabs.last().map(|tab| tab.id);
        }
        cx.notify();
    }
}

impl Render for RuLinkApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xd6d6d6))
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
    }
}

impl RuLinkApp {
    /// 左侧栏：标题、新建按钮，以及主机列表 / 新建表单 / 密码面板三选一。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = if self.new_host.is_some() {
            self.render_form_panel(cx)
        } else if self.pending.is_some() {
            self.render_pending_panel(cx)
        } else {
            self.render_host_list(cx)
        };

        v_flex()
            .w(px(280.0))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(0x252526))
            .border_r_1()
            .border_color(rgb(0x3a3a3a))
            .p_3()
            .gap_2()
            .child(
                div()
                    .text_lg()
                    .font_weight(rgpui::FontWeight(700.0))
                    .child("RuLink"),
            )
            .child(
                Button::new("new-host")
                    .label("+ 新建主机")
                    .primary()
                    .w_full()
                    .on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))),
            )
            .child(div().flex_1().overflow_y_scrollbar().child(panel))
    }

    /// 主机账号列表；每行可点击连接、附删除按钮。
    fn render_host_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.hosts.is_empty() {
            return div()
                .text_sm()
                .text_color(rgb(0x8a8a8a))
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
                    .bg(rgb(0x2d2d30))
                    .hover(|style| style.bg(rgb(0x37373d)))
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
                            .child(div().text_xs().text_color(rgb(0x9a9a9a)).child(host.addr())),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x6fa8dc))
                                    .child(auth_label(host.auth)),
                            )
                            .child(
                                div()
                                    .id(del_id)
                                    .text_sm()
                                    .text_color(rgb(0xbf5b5b))
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
            .bg(rgb(0x2d2d30))
            .child(labeled_field("展示名", &form.name))
            .child(labeled_field("主机地址", &form.host))
            .child(labeled_field("端口", &form.port))
            .child(labeled_field("用户名", &form.user))
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
                                rgb(0x0e639c)
                            } else {
                                rgb(0x3a3a3a)
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
                this.child(labeled_field("私钥路径", &form.key_path))
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
        let Some(pending) = self.pending.as_ref() else {
            return div().into_any_element();
        };
        let host = pending.host.addr();
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(rgb(0x2d2d30))
            .child(
                div()
                    .text_sm()
                    .child(format!("连接 {host} 需要密码（仅本次使用，不保存）")),
            )
            .child(styled_input(
                Input::new(&pending.password).content_type(InputContentType::Password),
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
        let tab_bar = if self.tabs.is_empty() {
            div()
        } else {
            h_flex()
                .h(px(36.0))
                .flex_shrink_0()
                .items_center()
                .bg(rgb(0x252526))
                .border_b_1()
                .border_color(rgb(0x3a3a3a))
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
                        .bg(if active { rgb(0x1e1e1e) } else { rgb(0x2d2d30) })
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
                                .text_color(rgb(0xbf5b5b))
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
                            .text_color(rgb(0x7a7a7a))
                            .child("从左侧点击主机账号开始连接"),
                    )
                    .into_any_element()
            });

        v_flex().flex_1().h_full().child(tab_bar).child(body)
    }
}

/// 带标题的输入框行。
fn labeled_field(label: &'static str, input: &Entity<InputState>) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().text_sm().child(label))
        .child(styled_input(Input::new(input)))
}

/// 统一输入框外观：暗底 + 明显边框 + 亮色文字，保证未聚焦时文字与边框都清晰可辨。
fn styled_input(input: Input) -> Input {
    input
        .bg(rgb(0x1b1b1c))
        .text_color(rgb(0xf2f2f2))
        .border_1()
        .border_color(rgb(0x4a4a4a))
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
    });
}
