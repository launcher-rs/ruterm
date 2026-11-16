//! 命令历史：右侧独立面板（搜索 + 复用）与完整管理弹窗。

use rgpui::input_ui::{Input, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, ContextMenuExt, Disableable, Entity, FontWeight,
    IconName, InteractiveElement, IntoElement, MouseButton, ParentElement, ScrollableElement,
    SharedString, Styled, div, h_flex, px, v_flex,
};

use crate::tr;

use super::*;

/// 命令历史弹窗状态。
pub(super) struct HistoryManager {
    pub(super) search: Entity<InputState>,
    /// 清空历史是否已进入确认步（再点一次才真清）。
    pub(super) confirm_clear: bool,
}

impl RuTermApp {
    /// 窗口右侧的命令历史面板：搜索 + 命令列表 + 底栏（管理 / 清空）。
    ///
    /// 窄面板放不下弹窗里那一排按钮，行内只留「单击插入」和悬浮的 ▶ 执行；
    /// 存为片段与删除收进右键菜单，完整管理交给「管理…」打开的弹窗。
    /// 面板宽度显式传入：空列表时 Taffy 会按内容宽度量这一列，
    /// 让 100% 宽度的搜索框/标题栏跟着塌掉；给死宽度就不依赖百分比解析。
    pub(super) fn render_history_panel(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let query = self.history_search.read(cx).value().trim().to_lowercase();
        let can_send = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .is_some_and(|tab| !tab.ended && tab.terminal_view.is_some());
        let total = self.history.entries().len();

        let mut list = v_flex()
            .id("history-panel-list")
            .flex_1()
            .min_h_0()
            .gap_px()
            .overflow_y_scrollbar();
        if self.history.is_empty() {
            list = list.child(
                div()
                    .py_6()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "还没有命令历史。在终端里执行命令后会自动记录。")),
            );
        }
        let mut shown = 0;
        for (index, command) in self.history.entries().iter().enumerate() {
            if !query.is_empty() && !command.to_lowercase().contains(&query) {
                continue;
            }
            shown += 1;
            list = list.child(self.render_history_panel_row(index, command, &pal, can_send, cx));
        }
        if !query.is_empty() && shown == 0 {
            list = list.child(
                div()
                    .py_4()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "没有匹配的历史命令")),
            );
        }

        let summary = if query.is_empty() {
            tr!(cx, "共 {count} 条，新的在前。", count = total)
        } else {
            tr!(
                cx,
                "匹配 {shown} 条，共 {total} 条。",
                shown = shown,
                total = total
            )
        };
        let confirming = self.history_clear_confirm;
        v_flex()
            .id("history-panel")
            .w(px(width))
            .h_full()
            .min_w_0()
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
                            .text_base()
                            .font_weight(FontWeight(600.0))
                            .child(tr!(cx, "命令历史")),
                    )
                    .child(
                        Button::new("history-panel-close")
                            .icon(IconName::Close)
                            .ghost()
                            .compact()
                            .tooltip(tr!(cx, "关闭历史面板"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_history_panel(cx);
                            })),
                    ),
            )
            .child(styled_input(Input::new(&self.history_search), &pal))
            .child(div().text_xs().text_color(pal.muted).child(summary))
            .child(list)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(tr!(cx, "单击插入 · ▶ 执行 · 右键更多")),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_shrink_0()
                            .child(
                                Button::new("history-panel-manage")
                                    .label(tr!(cx, "管理…"))
                                    .ghost()
                                    .compact()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_history_manager_impl(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("history-panel-clear")
                                    .label(if confirming {
                                        tr!(cx, "确认清空")
                                    } else {
                                        tr!(cx, "清空")
                                    })
                                    .ghost()
                                    .compact()
                                    .disabled(total == 0)
                                    .when(confirming, |button| button.text_color(pal.danger))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.request_clear_history_panel(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_history_panel_row(
        &self,
        index: usize,
        command: &str,
        pal: &Palette,
        can_send: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let menu_entity = cx.entity();
        h_flex()
            .id(("history-panel-row", index))
            .w_full()
            .h(px(28.0))
            .items_center()
            .gap_1()
            .pl_2()
            .pr_1()
            .rounded(px(4.0))
            .hover(|style| style.bg(pal.card_hover))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.use_history_entry(index, false, window, cx);
            }))
            .context_menu(move |menu, _, cx| {
                let insert = menu_entity.clone();
                let run = menu_entity.clone();
                let save = menu_entity.clone();
                let delete = menu_entity.clone();
                menu.min_w(px(168.0))
                    .item(Self::menu_item(
                        insert,
                        tr!(cx, "插入到当前会话"),
                        None,
                        false,
                        false,
                        move |this, window, cx| {
                            this.use_history_entry(index, false, window, cx);
                        },
                    ))
                    .item(
                        Self::menu_item(
                            run,
                            tr!(cx, "执行（回车）"),
                            Some(IconName::Play),
                            false,
                            false,
                            move |this, window, cx| {
                                this.use_history_entry(index, true, window, cx);
                            },
                        )
                        .disabled(!can_send),
                    )
                    .separator()
                    .item(Self::menu_item(
                        save,
                        tr!(cx, "存为片段…"),
                        None,
                        false,
                        false,
                        move |this, window, cx| {
                            this.save_history_as_snippet(index, window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        delete,
                        tr!(cx, "删除"),
                        Some(IconName::Delete),
                        true,
                        false,
                        move |this, _, cx| {
                            this.delete_history_entry(index, cx);
                        },
                    ))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(pal.text)
                    .text_ellipsis()
                    .child(SharedString::from(command.to_string())),
            )
            .when(can_send, |row| {
                row.child(
                    Button::new(("history-panel-run", index))
                        .icon(IconName::Play)
                        .ghost()
                        .xsmall()
                        .compact()
                        .tooltip(tr!(cx, "发送到当前会话并回车"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| {
                                // 别让 ▶ 的点击冒泡到整行，否则会再插入一遍。
                                cx.stop_propagation();
                            }),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.use_history_entry(index, true, window, cx);
                        })),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_history_manager(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.history_manager.as_ref() else {
            return div().into_any_element();
        };
        let total = self.history.entries().len();
        let subtitle = crate::i18n::translate(
            cx,
            "只记录终端回显过的命令行，密码输入不会被记录；共 {count} 条，新的在前。",
            &[("count", &total.to_string())],
        );
        let can_send = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .is_some_and(|tab| !tab.ended && tab.terminal_view.is_some());
        v_flex()
            .id("history-manager-backdrop")
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
                    .id("history-manager")
                    .key_context("CommandHistory")
                    .w(px(640.0))
                    .h(px(500.0))
                    .p_5()
                    .gap_3()
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
                                v_flex()
                                    .gap(px(2.0))
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(FontWeight(600.0))
                                            .child(tr!(cx, "命令历史")),
                                    )
                                    .child(div().text_xs().text_color(pal.muted).child(subtitle)),
                            )
                            .child(
                                Button::new("close-history-manager")
                                    .label("×")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_history_manager(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex().w_full().gap_2().items_center().child(
                            div()
                                .flex_1()
                                .child(styled_input(Input::new(&state.search), &pal)),
                        ),
                    )
                    .child(self.render_history_list(state, &pal, can_send, cx))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(pal.muted)
                                    .child(tr!(cx, "「执行」会自动追加回车；历史随加密库保存。")),
                            )
                            .child(
                                Button::new("history-clear")
                                    .label(if state.confirm_clear {
                                        tr!(cx, "确认清空")
                                    } else {
                                        tr!(cx, "清空历史")
                                    })
                                    .ghost()
                                    .compact()
                                    .disabled(total == 0)
                                    .when(state.confirm_clear, |button| {
                                        button.text_color(pal.danger)
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.request_clear_history(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_history_list(
        &self,
        state: &HistoryManager,
        pal: &Palette,
        can_send: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let query = state.search.read(cx).value().trim().to_lowercase();
        let mut list = v_flex()
            .id("history-list")
            .flex_1()
            .min_h_0()
            .gap_1()
            .overflow_y_scrollbar();
        if self.history.is_empty() {
            list = list.child(
                div()
                    .py_6()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "还没有命令历史。在终端里执行命令后会自动记录。")),
            );
        }
        let mut shown = 0;
        for (index, command) in self.history.entries().iter().enumerate() {
            if !query.is_empty() && !command.to_lowercase().contains(&query) {
                continue;
            }
            shown += 1;
            list = list.child(
                h_flex()
                    .id(("history-row", index))
                    .w_full()
                    .items_center()
                    .gap_3()
                    .p_2()
                    .rounded(px(5.0))
                    .hover(|style| style.bg(pal.card_hover))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(pal.text)
                            .text_ellipsis()
                            .child(SharedString::from(command.clone())),
                    )
                    .child(
                        Button::new(("history-insert", index))
                            .label(tr!(cx, "插入"))
                            .ghost()
                            .compact()
                            .disabled(!can_send)
                            .tooltip(tr!(cx, "插入到当前会话（不追加回车）"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_history_command(index, false, cx);
                            })),
                    )
                    .child(
                        Button::new(("history-run", index))
                            .label(tr!(cx, "执行"))
                            .ghost()
                            .compact()
                            .disabled(!can_send)
                            .tooltip(tr!(cx, "发送到当前会话并回车"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_history_command(index, true, cx);
                            })),
                    )
                    .child(
                        Button::new(("history-save", index))
                            .label(tr!(cx, "存为片段"))
                            .ghost()
                            .compact()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.save_history_as_snippet(index, window, cx);
                            })),
                    )
                    .child(
                        Button::new(("history-delete", index))
                            .label(tr!(cx, "删除"))
                            .ghost()
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_history_entry(index, cx);
                            })),
                    ),
            );
        }
        if !query.is_empty() && shown == 0 {
            list = list.child(
                div()
                    .py_4()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "没有匹配的历史命令")),
            );
        }
        list.into_any_element()
    }
}
