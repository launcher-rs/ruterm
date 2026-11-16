//! //! 端口转发页：规则保存与开关启停、活动转发的建立 / 撤除与面板渲染。

use rgpui::input_ui::{Input, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, App, Button, ButtonVariants, ClickEvent, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, ScrollableElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, h_flex, px, v_flex,
};

use crate::OpenForward;
use crate::tr;

use super::*;

/// 端口转发表单：默认是规则列表，点「设置…」才展开新增 / 编辑表单。
pub(super) struct ForwardDialog {
    /// 目标会话标签 id（转发指令发往该标签的 SSH 连接）。
    pub(super) tab_id: usize,
    /// 表单标题里的会话名。
    pub(super) title: SharedString,
    /// 该会话的连接键（user@host:port），保存规则时按它归档。
    pub(super) addr: SharedString,
    /// 表单是否展开（列表与新增分开，避免列表被表单挤占）。
    pub(super) editing: bool,
    /// 当前方向（-L / -R / -D），切换只换标签语义，不改输入内容。
    pub(super) kind: crate::session::ForwardKind,
    /// 新规则的备注名（可空）。
    pub(super) name: Entity<InputState>,
    pub(super) listen_port: Entity<InputState>,
    pub(super) target_host: Entity<InputState>,
    pub(super) target_port: Entity<InputState>,
    /// 新规则保存时是否带「连接时自动开启」。
    pub(super) auto_start: bool,
    pub(super) error: Option<SharedString>,
    pub(super) info: Option<SharedString>,
}

/// 端口转发列表的开关：轨道 + 滑块，点击整块切换。
pub(super) fn forward_switch(
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

impl RuTermApp {
    /// Ctrl+Shift+F / 会话菜单：切到边栏「转发」页。
    pub(super) fn open_forward_action(
        &mut self,
        _: &OpenForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_forward(window, cx);
    }

    /// 打开转发分区：规则绑定当前 SSH 会话，不新开连接。
    pub(super) fn open_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay_open() {
            return;
        }
        let Some(id) = self.active_tab else {
            self.status = "没有可转发的会话，请先连接 SSH 主机".into();
            cx.notify();
            return;
        };
        // 已经为这个会话准备好规则面板：只切分区，不重置正在编辑的表单。
        if self
            .forward
            .as_ref()
            .is_some_and(|dialog| dialog.tab_id == id)
        {
            self.show_sidebar_page(SidebarPage::Forward, cx);
            cx.notify();
            return;
        }
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
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "备注名（可选）")));
        let listen_port = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("8080")
                .placeholder(tr!(cx, "监听端口"))
        });
        let target_host = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("127.0.0.1")
                .placeholder(tr!(cx, "目标主机"))
        });
        let target_port = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("80")
                .placeholder(tr!(cx, "目标端口"))
        });
        listen_port.update(cx, |state, cx| state.focus(window, cx));
        self.show_sidebar_page(SidebarPage::Forward, cx);
        self.forward = Some(ForwardDialog {
            tab_id: id,
            title,
            addr,
            editing: false,
            kind: crate::session::ForwardKind::Local,
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
    pub(super) fn set_forward_kind(
        &mut self,
        kind: crate::session::ForwardKind,
        cx: &mut Context<Self>,
    ) {
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
            crate::session::ForwardKind::Local => {
                "本地转发：监听本机回环端口，目标按服务器视角解析".into()
            }
            crate::session::ForwardKind::Remote => {
                "远程转发：服务器只监听其回环端口，目标按本机视角解析".into()
            }
            crate::session::ForwardKind::Dynamic => {
                "动态转发：本机回环端口提供 SOCKS5 代理，目标由客户端请求决定".into()
            }
        };
        cx.notify();
    }

    /// 展开 / 收起新增规则表单；列表与添加分开，避免互相挤占。
    pub(super) fn set_forward_editing(&mut self, editing: bool, cx: &mut Context<Self>) {
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
    pub(super) fn set_forward_auto_start(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(dialog) = self.forward.as_mut() {
            dialog.auto_start = on;
        }
        cx.notify();
    }

    /// 关闭转发表单；已开启的转发继续在后台工作，可在再次打开时停止。
    pub(super) fn close_forward(&mut self, cx: &mut Context<Self>) {
        self.forward = None;
        cx.notify();
    }

    /// 校验表单字段，折算成规则（弹窗名称与自动开关一并返回）。
    pub(super) fn forward_form_rule(
        &self,
        dialog: &ForwardDialog,
        cx: &Context<Self>,
    ) -> Result<(crate::session::ForwardRule, String, bool), String> {
        let parse = |text: &str, label: &str| {
            text.parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| format!("{label}无效（应为 1–65535）"))
        };
        let listen_label = match dialog.kind {
            crate::session::ForwardKind::Local => "本地端口",
            crate::session::ForwardKind::Remote => "服务器监听端口",
            crate::session::ForwardKind::Dynamic => "SOCKS5 端口",
        };
        let listen_port = dialog.listen_port.read(cx).value().trim().to_string();
        let listen_port = parse(&listen_port, listen_label)?;
        let name = dialog.name.read(cx).value().trim().to_string();
        if dialog.kind == crate::session::ForwardKind::Dynamic {
            // 动态转发的目标由 SOCKS5 客户端逐条给出，表单里不填目标。
            return Ok((
                crate::session::ForwardRule {
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
            crate::session::ForwardKind::Local => {
                ("远端端口", "请填写远端主机（服务器视角的地址）")
            }
            _ => ("本地目标端口", "请填写本地目标主机（本机视角的地址）"),
        };
        let target_host = dialog.target_host.read(cx).value().trim().to_string();
        let target_port = dialog.target_port.read(cx).value().trim().to_string();
        let target_port = parse(&target_port, port_label)?;
        if target_host.is_empty() {
            return Err(host_hint.to_string());
        }
        Ok((
            crate::session::ForwardRule {
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
    pub(super) fn save_forward(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn start_forward_rule(
        &mut self,
        tab_id: usize,
        rule: crate::session::ForwardRule,
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
            .send(crate::session::SessionCommand::ForwardStart {
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
                Ok(result) => result.map_err(|error| crate::session::error_summary(&error)),
                Err(_) => Err("会话已断开，无法开启端口转发".to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                this.finish_forward_start(tab_id, result, cx);
            });
        })
        .detach();
    }

    /// 已保存规则的开关：开＝立即启动，关＝停止（规则本身保留）。
    pub(super) fn toggle_saved_forward(
        &mut self,
        saved: SavedForward,
        on: bool,
        cx: &mut Context<Self>,
    ) {
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
    pub(super) fn delete_saved_forward(&mut self, saved: SavedForward, cx: &mut Context<Self>) {
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
    pub(super) fn auto_start_forwards(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let Some(addr) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.addr.to_string())
        else {
            return;
        };
        let rules: Vec<crate::session::ForwardRule> = self
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
    pub(super) fn finish_forward_start(
        &mut self,
        tab_id: usize,
        result: Result<crate::session::ForwardRule, String>,
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
    pub(super) fn stop_forward(
        &mut self,
        rule: crate::session::ForwardRule,
        cx: &mut Context<Self>,
    ) {
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
            .send(crate::session::SessionCommand::ForwardStop {
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
    pub(super) fn remove_forward_rule(
        &mut self,
        tab_id: usize,
        rule: &crate::session::ForwardRule,
    ) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            let id = rule.id();
            tab.forwards.retain(|existing| existing.id() != id);
        }
    }

    /// 端口转发的即时错误：写进弹窗（若还开着）并同步状态栏。
    pub(super) fn forward_error(
        &mut self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let message = message.into();
        if let Some(dialog) = self.forward.as_mut() {
            dialog.error = Some(message.clone());
            dialog.info = None;
        }
        self.status = message;
        cx.notify();
    }

    /// 边栏「转发」分区：已保存规则的开关列表 + 新建规则表单。
    pub(super) fn render_forward_section(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(dialog) = self.forward.as_ref() else {
            return v_flex()
                .id("forward-empty")
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .gap_2()
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .text_color(pal.muted)
                        .child(tr!(cx, "没有打开的转发配置")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(tr!(cx, "转发规则绑定当前 SSH 会话，保存后用开关即时启停")),
                )
                .child(
                    Button::new("forward-start")
                        .label(tr!(cx, "设置转发…"))
                        .primary()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_forward(window, cx);
                        })),
                )
                .into_any_element();
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
            crate::session::ForwardKind::Local => (
                tr!(cx, "本地端口"),
                tr!(cx, "远端主机"),
                tr!(cx, "远端端口"),
                tr!(
                    cx,
                    "监听只绑本机 127.0.0.1，不对局域网开放；目标地址按服务器视角解析。"
                ),
            ),
            crate::session::ForwardKind::Remote => (
                tr!(cx, "服务器监听端口"),
                tr!(cx, "本地目标主机"),
                tr!(cx, "本地目标端口"),
                tr!(
                    cx,
                    "服务器只监听其 127.0.0.1，不对公网开放；目标地址按本机视角解析。"
                ),
            ),
            crate::session::ForwardKind::Dynamic => (
                tr!(cx, "SOCKS5 端口"),
                SharedString::default(),
                SharedString::default(),
                tr!(
                    cx,
                    "在本机 127.0.0.1 上提供 SOCKS5 代理（无认证），目标地址由客户端请求给出、经服务器解析。"
                ),
            ),
        };
        let is_dynamic = kind == crate::session::ForwardKind::Dynamic;
        let local_button = if kind == crate::session::ForwardKind::Local {
            Button::new("forward-kind-local")
                .label(tr!(cx, "本地 -L"))
                .primary()
        } else {
            Button::new("forward-kind-local")
                .label(tr!(cx, "本地 -L"))
                .ghost()
        }
        .on_click(cx.listener(|this, _, _, cx| {
            this.set_forward_kind(crate::session::ForwardKind::Local, cx);
        }));
        let remote_button = if kind == crate::session::ForwardKind::Remote {
            Button::new("forward-kind-remote")
                .label(tr!(cx, "远程 -R"))
                .primary()
        } else {
            Button::new("forward-kind-remote")
                .label(tr!(cx, "远程 -R"))
                .ghost()
        }
        .on_click(cx.listener(|this, _, _, cx| {
            this.set_forward_kind(crate::session::ForwardKind::Remote, cx);
        }));
        let dynamic_button = if is_dynamic {
            Button::new("forward-kind-dynamic")
                .label(tr!(cx, "动态 -D"))
                .primary()
        } else {
            Button::new("forward-kind-dynamic")
                .label(tr!(cx, "动态 -D"))
                .ghost()
        }
        .on_click(cx.listener(|this, _, _, cx| {
            this.set_forward_kind(crate::session::ForwardKind::Dynamic, cx);
        }));
        v_flex()
            .id("forward-section")
            .flex_1()
            .min_h_0()
            .w_full()
            .p_3()
            .gap_3()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .id("forward-form")
                    .w_full()
                    .gap_3()
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(div().text_base().font_weight(FontWeight(600.0)).child(tr!(
                                cx,
                                "端口转发 · {target}",
                                target = dialog.title
                            )))
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
                                    .child(tr!(cx, "已保存的转发（开关即时生效）")),
                            )
                            .child(
                                v_flex()
                                    .id("forward-saved-list")
                                    .gap_1()
                                    .max_h(if editing { px(150.0) } else { px(260.0) })
                                    .overflow_y_scrollbar()
                                    .when(saved_rules.is_empty(), |this| {
                                        this.child(div().text_sm().text_color(pal.muted).child(
                                            tr!(cx, "暂无保存的规则，点下方「设置…」添加一条。"),
                                        ))
                                    })
                                    .children(saved_rules.iter().map(|saved| {
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
                                                .label(tr!(cx, "删除"))
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
                                        .child(div().text_sm().child(tr!(cx, "名称")))
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
                                        .child(styled_input(Input::new(&dialog.listen_port), &pal)),
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
                                        .child(div().text_sm().child(tr!(cx, "连接时自动开启"))),
                                )
                                .child(div().flex_1())
                                .child(
                                    Button::new("forward-cancel")
                                        .label(tr!(cx, "取消"))
                                        .ghost()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_forward_editing(false, cx);
                                        })),
                                )
                                .child(
                                    Button::new("forward-save")
                                        .label(tr!(cx, "保存规则"))
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
                                        .label(tr!(cx, "设置…"))
                                        .secondary()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_forward_editing(true, cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(pal.muted)
                                        .child(tr!(cx, "添加或修改转发规则")),
                                )
                                .child(div().flex_1())
                                .child(
                                    Button::new("forward-close")
                                        .label(tr!(cx, "关闭"))
                                        .ghost()
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.close_forward(cx)),
                                        ),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    /// 端口转发规则落库；加密库不可用时退回旧 settings.json。
    pub(super) fn persist_forwards(&self, cx: &mut Context<Self>) {
        let json = self.forward_prefs.to_json();
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
