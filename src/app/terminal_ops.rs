//! //! 终端输入侧操作：查找条（Ctrl+F）、同步输入（MirrorInput 中间件）与命令广播。


use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Checkbox, Context, Disableable, Entity, Focusable, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    Styled, Window, div, h_flex, px, v_flex,
};
use rgpui_term::{
    InputOrigin,
    TerminalMiddleware, TerminalView,
};

use crate::{FindInTerminal, CloseFind, ToggleSyncInput, BroadcastCommand, CancelBroadcast};
use crate::tr;

use super::*;

/// 终端输入镜像中间件：把用户敲进活动终端的字节转发给应用，供「同步输入」分发。
///
/// 只观察不改写（始终原样放行）。`Programmatic` 来源正是同步写回产生的输入，必须
/// 忽略，否则几个会话之间会来回镜像、无限放大。
pub(super) struct MirrorInput {
    pub(super) tab_id: usize,
    pub(super) tx: tokio::sync::mpsc::UnboundedSender<(usize, InputOrigin, Vec<u8>)>,
}

impl TerminalMiddleware for MirrorInput {
    fn on_input(
        &self,
        input: std::borrow::Cow<'static, [u8]>,
        origin: InputOrigin,
    ) -> Option<std::borrow::Cow<'static, [u8]>> {
        // 只镜像"人输入"的三类：键盘、文本上屏、粘贴。鼠标上报与滚动属于
        // 具体会话自己的交互，广播过去会污染其它程序。
        if matches!(
            origin,
            InputOrigin::Keystroke | InputOrigin::Text | InputOrigin::Paste
        ) && !input.is_empty()
        {
            let _ = self.tx.send((self.tab_id, origin, input.to_vec()));
        }
        Some(input)
    }
}

/// 命令广播弹窗：把一条命令以回车结尾写进所有活动会话（需二次确认）。
pub(super) struct BroadcastState {
    pub(super) input: Entity<InputState>,
    pub(super) confirm: bool,
}

/// 终端内查找条（Ctrl+F）的状态。
pub(super) struct FindBar {
    /// 关键词输入框；关闭查找时只收起、不销毁，下次打开沿用上次的关键词。
    pub(super) input: Entity<InputState>,
    /// 查找条当前是否展示。
    pub(super) visible: bool,
    /// 按正则匹配；关闭时按字面量匹配（所见即所搜）。
    pub(super) regex: bool,
    /// 最近一次下发给终端的关键词与匹配方式，避免每次重绘重复入队。
    pub(super) applied: Option<(String, bool)>,
}

impl RuTermApp {
    /// Ctrl+F / 菜单「终端内查找」：展开查找条并聚焦关键词输入框。
    ///
    /// 沿用上次的关键词并全选：直接敲字就替换，回车接着在旧词上跳转。
    pub(super) fn find_in_terminal(
        &mut self,
        _: &FindInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_tab.is_none() {
            self.status = "当前没有可查找的终端".into();
            cx.notify();
            return;
        }
        rdlog("find_in_terminal OPEN");
        self.find.visible = true;
        self.find.input.update(cx, |state, cx| {
            let len = state.value().len();
            state.set_selected_range(0..len, cx);
            state.focus(window, cx);
        });
        // 新终端要重新下发一次关键词，不能沿用 applied 的去重判断。
        self.find.applied = None;
        self.apply_find(cx);
        cx.notify();
    }


    pub(super) fn close_find(&mut self, _: &CloseFind, window: &mut Window, cx: &mut Context<Self>) {
        self.do_close_find(window, cx);
    }


    /// 收起查找条：清掉高亮与定位，焦点交回活动终端。
    pub(super) fn do_close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find.visible {
            return;
        }
        self.find.visible = false;
        self.find.applied = None;
        if let Some(view) = self.active_terminal_view() {
            // 空串＝清除搜索：高亮与滚动定位一并收回。
            view.update(cx, |view, cx| view.search("", cx));
            let focus = view.read(cx).focus_handle(cx);
            focus.focus(window, cx);
        }
        cx.notify();
    }


    /// 活动标签的终端视图；无标签时为 None。
    pub(super) fn active_terminal_view(&self) -> Option<Entity<TerminalView>> {
        let id = self.active_tab?;
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.terminal_view.clone())
    }


    /// 把查找条的关键词下发给活动终端（关键词与匹配方式都没变就不重复入队）。
    pub(super) fn apply_find(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.active_terminal_view() else {
            return;
        };
        let query = self.find.input.read(cx).value().to_string();
        let regex = self.find.regex;
        if self
            .find
            .applied
            .as_ref()
            .is_some_and(|(last, last_regex)| *last == query && *last_regex == regex)
        {
            return;
        }
        self.find.applied = Some((query.clone(), regex));
        view.update(cx, |view, cx| {
            if query.is_empty() {
                view.search("", cx);
            } else if regex {
                view.search(&query, cx);
            } else {
                view.search_literal(&query, cx);
            }
        });
    }


    /// 回车 / Shift+回车（或上/下按钮）：在匹配之间跳转。
    pub(super) fn step_find(&mut self, backward: bool, cx: &mut Context<Self>) {
        let Some(view) = self.active_terminal_view() else {
            return;
        };
        if self.find.input.read(cx).value().is_empty() {
            return;
        }
        let terminal = view.read(cx).terminal().clone();
        terminal.update(cx, |terminal, _| {
            if backward {
                terminal.search_previous();
            } else {
                terminal.search_next();
            }
        });
    }


    /// 可接收输入的会话数（不含已结束 / 连接失败的标签）。
    pub(super) fn live_tab_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|tab| !tab.ended && !tab.failed)
            .count()
    }


    /// 同步输入开关：打开后活动终端里的按键会同时写进其它会话。
    pub(super) fn toggle_sync_input(
        &mut self,
        _: &ToggleSyncInput,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sync_input(!self.sync_input, cx);
    }


    pub(super) fn set_sync_input(&mut self, on: bool, cx: &mut Context<Self>) {
        let live = self.live_tab_count();
        if on && live < 2 {
            self.status = "同步输入至少需要两个可用会话".into();
            cx.notify();
            return;
        }
        self.sync_input = on;
        self.status = if on {
            SharedString::from(format!("同步输入：开（{live} 个会话）"))
        } else {
            "同步输入：关".into()
        };
        cx.notify();
    }


    /// 中间件送来的输入：只转发活动终端的输入，写到其它未结束的会话。
    pub(super) fn mirror_input(
        &mut self,
        source: usize,
        origin: InputOrigin,
        bytes: Vec<u8>,
        cx: &mut Context<Self>,
    ) {
        if !self.sync_input || self.active_tab != Some(source) {
            return;
        }
        // 粘贴带的是本终端按自身 bracketed-paste 模式包好的字节；别的会话未必开了
        // 同一模式，直接发过去会露出转义序列。还原成纯文本，按目标自己的能力处理。
        let payload = if origin == InputOrigin::Paste {
            match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    let inner = text
                        .strip_prefix("\x1b[200~")
                        .and_then(|rest| rest.strip_suffix("\x1b[201~"))
                        .unwrap_or(text);
                    inner.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
                }
                Err(_) => bytes.clone(),
            }
        } else {
            bytes.clone()
        };
        for tab in self.tabs.iter() {
            if tab.id == source || tab.ended || tab.failed {
                continue;
            }
            let terminal = tab.terminal_view.read(cx).terminal().clone();
            // 走 `input()`＝Programmatic 来源，中间件不再镜像，避免回流。
            terminal.update(cx, |terminal, _| terminal.input(payload.clone()));
        }
    }


    /// 菜单「命令发到所有会话…」：弹出输入框，确认后统一发送。
    pub(super) fn broadcast_command(
        &mut self,
        _: &BroadcastCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.live_tab_count() == 0 {
            self.status = "没有可发送命令的会话".into();
            cx.notify();
            return;
        }
        if let Some(state) = self.broadcast.as_ref() {
            state.input.update(cx, |state, cx| state.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!(cx, "要发送到所有会话的命令（回车在远端执行）"))
        });
        cx.subscribe(&input, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        input.update(cx, |state, cx| state.focus(window, cx));
        self.broadcast = Some(BroadcastState {
            input,
            confirm: false,
        });
        cx.notify();
    }


    pub(super) fn close_broadcast(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.broadcast.take().is_none() {
            return;
        }
        if let Some(view) = self.active_terminal_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }


    pub(super) fn cancel_broadcast(
        &mut self,
        _: &CancelBroadcast,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_broadcast(window, cx);
    }


    /// 确认发送：命令 + 回车写进每个活动会话，等效于手动逐台输入。
    pub(super) fn commit_broadcast(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.broadcast.as_ref() else {
            return;
        };
        if !state.confirm {
            self.status = "请先勾选确认再发送".into();
            cx.notify();
            return;
        }
        let command = state
            .input
            .read(cx)
            .value()
            .trim_end_matches(['\r', '\n'])
            .to_string();
        if command.is_empty() {
            self.status = "命令不能为空".into();
            cx.notify();
            return;
        }
        let mut bytes = command.into_bytes();
        bytes.push(b'\r');
        let mut targets = 0;
        for tab in self.tabs.iter() {
            if tab.ended || tab.failed {
                continue;
            }
            let terminal = tab.terminal_view.read(cx).terminal().clone();
            terminal.update(cx, |terminal, _| terminal.input(bytes.clone()));
            targets += 1;
        }
        self.broadcast = None;
        self.status = SharedString::from(format!("已向 {targets} 个会话发送命令"));
        if let Some(view) = self.active_terminal_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }


    /// 命令广播弹窗：列清目标会话，勾选确认后才能发送。
    pub(super) fn render_broadcast_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.broadcast.as_ref() else {
            return div().into_any_element();
        };
        let live: Vec<&SessionTab> = self
            .tabs
            .iter()
            .filter(|tab| !tab.ended && !tab.failed)
            .collect();
        let count = live.len();
        let names = live
            .iter()
            .map(|tab| tab.title.to_string())
            .collect::<Vec<_>>()
            .join("、");
        let confirm_entity = cx.entity();
        v_flex()
            .id("broadcast-backdrop")
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
                    .id("broadcast-dialog")
                    .key_context("BroadcastDialog")
                    .w(px(480.0))
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
                                    .child("命令发到所有会话"),
                            )
                            .child(Button::new("close-broadcast").label("×").ghost().on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.close_broadcast(window, cx);
                                }),
                            )),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child("命令"))
                            .child(styled_input(Input::new(&state.input), &pal)),
                    )
                    .child(div().text_xs().text_color(pal.muted).child(format!(
                        "将向 {count} 个可用会话发送：{names}。已结束或连接失败的会话不在其中。"
                    )))
                    .child(
                        Checkbox::new("broadcast-confirm")
                            .label(format!("我确认：该命令会在 {count} 个会话中立即执行"))
                            .checked(state.confirm)
                            .on_change(move |on, _, cx| {
                                confirm_entity.update(cx, |this, cx| {
                                    if let Some(state) = this.broadcast.as_mut() {
                                        state.confirm = on;
                                    }
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.danger)
                            .child("命令以回车结尾写入，与手动在终端里输入等效，发出后无法撤回。"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("send-broadcast")
                                    .label("发送")
                                    .primary()
                                    .disabled(!state.confirm)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.commit_broadcast(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("cancel-broadcast")
                                    .label("取消")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_broadcast(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }


}
