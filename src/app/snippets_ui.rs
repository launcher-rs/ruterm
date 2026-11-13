//! 命令片段管理器：列表（搜索 / 插入 / 执行 / 编辑 / 删除）+ 编辑表单。
//!
//! 数据与校验在 `crate::snippets`（含单测）；发送走 `Terminal::input`
//! （Programmatic，不回流同步输入的镜像中间件）。

use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, Disableable, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, ScrollableElement, SharedString, Styled,
    Window, div, h_flex, px, v_flex,
};

use crate::snippets::CommandSnippet;
use crate::tr;
use crate::{CloseSnippetManager, SnippetManager as OpenSnippetManager};

use super::*;

/// 片段管理器弹窗状态：列表 ⇄ 编辑表单二选一。
pub(super) struct SnippetManager {
    pub(super) search: Entity<InputState>,
    /// None = 列表视图；Some = 正在新建 / 编辑。
    pub(super) editing: Option<SnippetEdit>,
    /// 待确认删除的下标（再点一次「删除」才真删）。
    pub(super) confirm_delete: Option<usize>,
    /// 列表是否切到「内置命令」页（默认显示自己的片段）。
    pub(super) builtin: bool,
}

pub(super) struct SnippetEdit {
    /// 正在编辑的片段下标；None = 新建。
    pub(super) index: Option<usize>,
    pub(super) name: Entity<InputState>,
    pub(super) command: Entity<InputState>,
    pub(super) note: Entity<InputState>,
    pub(super) error: Option<SharedString>,
}

impl RuTermApp {
    /// Ctrl+Shift+S /「终端」菜单：打开命令片段管理器。
    pub(super) fn open_snippet_manager(
        &mut self,
        _: &OpenSnippetManager,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_snippet_manager_impl(window, cx);
    }

    /// 打开管理器；已打开时只把焦点放回搜索框。
    pub(super) fn open_snippet_manager_impl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.snippet_manager.as_ref() {
            state.search.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
            return;
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "搜索命令片段…")));
        cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        search.update(cx, |input, cx| input.focus(window, cx));
        self.snippet_manager = Some(SnippetManager {
            search,
            editing: None,
            confirm_delete: None,
            builtin: false,
        });
        cx.notify();
    }

    /// 切换「我的片段 / 内置命令」两页。
    pub(super) fn set_snippet_tab(&mut self, builtin: bool, cx: &mut Context<Self>) {
        if let Some(manager) = self.snippet_manager.as_mut()
            && manager.builtin != builtin
        {
            manager.builtin = builtin;
            manager.confirm_delete = None;
            cx.notify();
        }
    }

    pub(super) fn close_snippet_manager_action(
        &mut self,
        _: &CloseSnippetManager,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_snippet_manager(window, cx);
    }

    pub(super) fn close_snippet_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_manager.take().is_none() {
            return;
        }
        if let Some(view) = self.active_terminal_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// 片段持久化：加密库 `settings.snippets`，库不可用时降级 `settings.json`。
    pub(super) fn persist_snippets(&self, cx: &mut Context<Self>) {
        let json = crate::snippets::to_json(&self.snippets);
        let Some(vault) = self.vault.clone() else {
            self.store.save_snippets(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("snippets", &json).await {
                log::warn!("写入库内命令片段失败: {error}");
            }
        })
        .detach();
    }

    /// 新建 / 编辑共用的表单构造。
    fn make_snippet_edit(
        index: Option<usize>,
        name: &str,
        command: &str,
        note: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SnippetEdit {
        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(name.to_string())
                .placeholder(tr!(cx, "例如：重启 nginx"))
        });
        let command_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(command.to_string())
                .placeholder(tr!(cx, "例如：systemctl restart nginx"))
        });
        let note_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(note.to_string())
                .placeholder(tr!(cx, "备注用于搜索，不会发送"))
        });
        SnippetEdit {
            index,
            name: name_input,
            command: command_input,
            note: note_input,
            error: None,
        }
    }

    pub(super) fn begin_snippet_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_snippet_edit_values(None, "", "", "", window, cx);
    }

    /// 打开编辑表单并预填内容：新建、复制内置命令、从历史存片段共用。
    pub(super) fn begin_snippet_edit_values(
        &mut self,
        index: Option<usize>,
        name: &str,
        command: &str,
        note: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_manager.as_ref().is_none() {
            return;
        }
        let edit = Self::make_snippet_edit(index, name, command, note, window, cx);
        let name_input = edit.name.clone();
        if let Some(manager) = self.snippet_manager.as_mut() {
            manager.editing = Some(edit);
            manager.confirm_delete = None;
        }
        name_input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn begin_snippet_edit(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snippet) = self.snippets.get(index).cloned() else {
            return;
        };
        self.begin_snippet_edit_values(
            Some(index),
            &snippet.name,
            &snippet.command,
            &snippet.note,
            window,
            cx,
        );
    }

    /// 把内置命令复制进「我的片段」编辑器；builtin 页保持不动，取消即回列表。
    pub(super) fn copy_preset_to_snippets(
        &mut self,
        preset_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(preset) = crate::snippet_presets::PRESETS.get(preset_index) else {
            return;
        };
        self.begin_snippet_edit_values(
            None,
            preset.name,
            preset.command,
            preset.note,
            window,
            cx,
        );
    }

    pub(super) fn cancel_snippet_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(manager) = self.snippet_manager.as_mut() {
            manager.editing = None;
            manager.confirm_delete = None;
        }
        cx.notify();
    }

    fn set_snippet_edit_error(&mut self, reason: &str, cx: &mut Context<Self>) {
        if let Some(manager) = self.snippet_manager.as_mut()
            && let Some(edit) = manager.editing.as_mut()
        {
            edit.error = Some(crate::i18n::translate(cx, reason, &[]));
        }
        cx.notify();
    }

    /// 保存表单：校验 → 原位替换或追加 → 持久化。
    pub(super) fn commit_snippet_edit(&mut self, cx: &mut Context<Self>) {
        let Some(manager) = self.snippet_manager.as_ref() else {
            return;
        };
        let Some(edit) = manager.editing.as_ref() else {
            return;
        };
        let name = edit.name.read(cx).value().to_string();
        let command = edit.command.read(cx).value().to_string();
        let note = edit.note.read(cx).value().to_string();
        let index = edit.index;
        let snippet = match crate::snippets::validate(&name, &command, &note) {
            Ok(snippet) => snippet,
            Err(reason) => {
                self.set_snippet_edit_error(reason, cx);
                return;
            }
        };
        match index {
            Some(ix) if ix < self.snippets.len() => self.snippets[ix] = snippet.clone(),
            _ => self.snippets.push(snippet.clone()),
        }
        if let Some(manager) = self.snippet_manager.as_mut() {
            manager.editing = None;
        }
        self.persist_snippets(cx);
        self.status = crate::i18n::translate(
            cx,
            "已保存命令片段：{name}",
            &[("name", &snippet.name)],
        );
        cx.notify();
    }

    /// 删除（两步确认：第一次只把按钮换成「确认删除」）。
    pub(super) fn request_delete_snippet(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(manager) = self.snippet_manager.as_mut() else {
            return;
        };
        if manager.confirm_delete != Some(index) {
            manager.confirm_delete = Some(index);
            cx.notify();
            return;
        }
        manager.confirm_delete = None;
        if index < self.snippets.len() {
            self.snippets.remove(index);
            self.persist_snippets(cx);
        }
        cx.notify();
    }

    /// 插入 / 执行到当前活动会话：插入只写命令文本，执行再补一个回车。
    pub(super) fn send_snippet(&mut self, index: usize, run: bool, cx: &mut Context<Self>) {
        let Some(snippet) = self.snippets.get(index).cloned() else {
            return;
        };
        if !self.send_command_to_terminal(&snippet.command, run, cx) {
            return;
        }
        let key = if run { "已执行片段：{name}" } else { "已插入片段：{name}" };
        self.status = crate::i18n::translate(cx, key, &[("name", &snippet.name)]);
        cx.notify();
    }

    /// 插入 / 执行内置命令：命令原样发送，用户可先「存为我的片段」再改参数。
    pub(super) fn send_preset(&mut self, index: usize, run: bool, cx: &mut Context<Self>) {
        let Some(preset) = crate::snippet_presets::PRESETS.get(index) else {
            return;
        };
        let name = crate::i18n::translate(cx, preset.name, &[]);
        if !self.send_command_to_terminal(preset.command, run, cx) {
            return;
        }
        let key = if run { "已执行内置命令：{name}" } else { "已插入内置命令：{name}" };
        self.status = crate::i18n::translate(cx, key, &[("name", name.as_ref())]);
        cx.notify();
    }

    /// 管理器弹窗：列表与编辑表单共用同一张卡片。
    pub(super) fn render_snippet_manager(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.snippet_manager.as_ref() else {
            return div().into_any_element();
        };
        let body = match state.editing.as_ref() {
            Some(edit) => self.render_snippet_editor(edit, &pal, cx),
            None if state.builtin => self.render_preset_list(state, &pal, cx),
            None => self.render_snippet_list(state, &pal, cx),
        };
        v_flex()
            .id("snippet-manager-backdrop")
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
                    .id("snippet-manager")
                    .key_context("SnippetManager")
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
                                            .child(tr!(cx, "命令片段")),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(pal.muted)
                                            .child(tr!(
                                                cx,
                                                "保存常用命令，按需插入或直接执行到当前会话。"
                                            )),
                                    ),
                            )
                            .child(
                                Button::new("close-snippet-manager")
                                    .label("×")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_snippet_manager(window, cx);
                                    })),
                            ),
                    )
                    .when(state.editing.is_none(), |this| {
                        this.child(self.render_snippet_tabs(state, &pal, cx))
                    })
                    .child(body),
            )
            .into_any_element()
    }

    /// 「我的片段 / 内置命令」两个页签。
    fn render_snippet_tabs(
        &self,
        state: &SnippetManager,
        _pal: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let tab = |id: &'static str, label: SharedString, active: bool, builtin: bool| {
            Button::new(id)
                .label(label)
                .compact()
                .when(active, |button| button.primary())
                .when(!active, |button| button.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_snippet_tab(builtin, cx);
                }))
        };
        h_flex()
            .gap_1()
            .child(tab(
                "snippet-tab-mine",
                tr!(cx, "我的片段"),
                !state.builtin,
                false,
            ))
            .child(tab(
                "snippet-tab-builtin",
                tr!(cx, "内置命令"),
                state.builtin,
                true,
            ))
            .into_any_element()
    }

    /// 内置命令页：按平台分组，只读；可插入 / 执行，或复制进「我的片段」再改。
    fn render_preset_list(
        &self,
        state: &SnippetManager,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let query = state.search.read(cx).value().trim().to_lowercase();
        let matches = |preset: &crate::snippet_presets::PresetSnippet| {
            query.is_empty()
                || preset.name.to_lowercase().contains(&query)
                || preset.command.to_lowercase().contains(&query)
                || preset.note.to_lowercase().contains(&query)
        };
        let can_send = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .is_some_and(|tab| !tab.ended && tab.terminal_view.is_some());
        let mut list = v_flex().id("preset-list").gap_1().overflow_y_scrollbar();
        let mut shown = 0;
        for category in crate::snippet_presets::PRESET_CATEGORIES {
            let rows: Vec<usize> = crate::snippet_presets::PRESETS
                .iter()
                .enumerate()
                .filter(|(_, preset)| preset.category == *category && matches(preset))
                .map(|(index, _)| index)
                .collect();
            if rows.is_empty() {
                continue;
            }
            list = list.child(
                div()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(crate::i18n::translate(cx, category, &[])),
            );
            for index in rows {
                shown += 1;
                let preset = &crate::snippet_presets::PRESETS[index];
                list = list.child(
                    h_flex()
                        .id(("preset-row", index))
                        .w_full()
                        .items_center()
                        .gap_3()
                        .p_2()
                        .rounded(px(5.0))
                        .hover(|style| style.bg(pal.card_hover))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(pal.text)
                                        .child(crate::i18n::translate(cx, preset.name, &[])),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(pal.muted)
                                        .text_ellipsis()
                                        .child(preset.command),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(pal.muted)
                                        .text_ellipsis()
                                        .child(crate::i18n::translate(cx, preset.note, &[])),
                                ),
                        )
                        .child(
                            Button::new(("preset-insert", index))
                                .label(tr!(cx, "插入"))
                                .ghost()
                                .compact()
                                .disabled(!can_send)
                                .tooltip(tr!(cx, "插入到当前会话（不追加回车）"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.send_preset(index, false, cx);
                                })),
                        )
                        .child(
                            Button::new(("preset-run", index))
                                .label(tr!(cx, "执行"))
                                .ghost()
                                .compact()
                                .disabled(!can_send)
                                .tooltip(tr!(cx, "发送到当前会话并回车"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.send_preset(index, true, cx);
                                })),
                        )
                        .child(
                            Button::new(("preset-copy", index))
                                .label(tr!(cx, "存为我的片段"))
                                .ghost()
                                .compact()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.copy_preset_to_snippets(index, window, cx);
                                })),
                        ),
                );
            }
        }
        if shown == 0 {
            list = list.child(
                div()
                    .py_4()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "没有匹配的内置命令")),
            );
        }
        v_flex()
            .gap_3()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(styled_input(Input::new(&state.search), pal))),
            )
            .child(list)
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(
                        cx,
                        "内置命令只读；需要固定参数时点「存为我的片段」再改。"
                    )),
            )
            .into_any_element()
    }

    /// 列表视图：搜索 + 每行（名称 / 命令 / 备注 / 插入 / 执行 / 编辑 / 删除）。
    fn render_snippet_list(
        &self,
        state: &SnippetManager,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let query = state.search.read(cx).value().trim().to_lowercase();
        let matches = |snippet: &CommandSnippet| {
            query.is_empty()
                || snippet.name.to_lowercase().contains(&query)
                || snippet.command.to_lowercase().contains(&query)
                || snippet.note.to_lowercase().contains(&query)
        };
        // 执行 / 插入都要求活动标签仍持有终端且未断开。
        let can_send = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .is_some_and(|tab| !tab.ended && tab.terminal_view.is_some());
        let mut list = v_flex().id("snippet-list").gap_1().overflow_y_scrollbar();
        if self.snippets.is_empty() {
            list = list.child(
                div()
                    .py_6()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "还没有命令片段。点「新建片段」添加一条。")),
            );
        }
        let mut shown = 0;
        for (index, snippet) in self.snippets.iter().enumerate() {
            if !matches(snippet) {
                continue;
            }
            shown += 1;
            let confirming = state.confirm_delete == Some(index);
            list = list.child(
                h_flex()
                    .id(("snippet-row", index))
                    .w_full()
                    .items_center()
                    .gap_3()
                    .p_2()
                    .rounded(px(5.0))
                    .hover(|style| style.bg(pal.card_hover))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(pal.text)
                                    .child(if snippet.has_name() {
                                        snippet.name.clone().into()
                                    } else {
                                        tr!(cx, "未命名片段")
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(pal.muted)
                                    .text_ellipsis()
                                    .child(snippet.command.clone()),
                            )
                            .when(!snippet.note.is_empty(), |row| {
                                row.child(
                                    div()
                                        .text_xs()
                                        .text_color(pal.muted)
                                        .text_ellipsis()
                                        .child(snippet.note.clone()),
                                )
                            }),
                    )
                    .child(
                        Button::new(("snippet-insert", index))
                            .label(tr!(cx, "插入"))
                            .ghost()
                            .compact()
                            .disabled(!can_send)
                            .tooltip(tr!(cx, "插入到当前会话（不追加回车）"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_snippet(index, false, cx);
                            })),
                    )
                    .child(
                        Button::new(("snippet-run", index))
                            .label(tr!(cx, "执行"))
                            .ghost()
                            .compact()
                            .disabled(!can_send)
                            .tooltip(tr!(cx, "发送到当前会话并回车"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_snippet(index, true, cx);
                            })),
                    )
                    .child(
                        Button::new(("snippet-edit", index))
                            .label(tr!(cx, "编辑"))
                            .ghost()
                            .compact()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.begin_snippet_edit(index, window, cx);
                            })),
                    )
                    .child(
                        Button::new(("snippet-delete", index))
                            .label(if confirming {
                                tr!(cx, "确认删除")
                            } else {
                                tr!(cx, "删除")
                            })
                            .ghost()
                            .compact()
                            .when(confirming, |button| button.text_color(pal.danger))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.request_delete_snippet(index, cx);
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
                    .child(tr!(cx, "没有匹配的命令片段")),
            );
        }
        v_flex()
            .gap_3()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(styled_input(Input::new(&state.search), pal)))
                    .child(
                        Button::new("snippet-new")
                            .label(tr!(cx, "新建片段"))
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.begin_snippet_new(window, cx);
                            })),
                    ),
            )
            .child(list)
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(
                        cx,
                        "片段保存在加密库；「执行」会自动追加回车。"
                    )),
            )
            .into_any_element()
    }

    /// 编辑视图：名称 / 命令 / 备注 / 错误提示 / 保存取消。
    fn render_snippet_editor(
        &self,
        edit: &SnippetEdit,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let error = match edit.error.as_deref() {
            Some(text) => crate::i18n::translate(cx, text, &[]),
            None => SharedString::from(""),
        };
        v_flex()
            .gap_3()
            .flex_1()
            .min_h_0()
            .child(labeled_field(
                match edit.index {
                    Some(_) => "名称（编辑）",
                    None => "名称",
                },
                &edit.name,
                pal,
            ))
            .child(labeled_field("命令", &edit.command, pal))
            .child(labeled_field("备注（可选）", &edit.note, pal))
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(
                        cx,
                        "单行命令；内容按字面发送，不做变量替换。"
                    )),
            )
            .child(div().text_xs().text_color(pal.danger).child(error))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("snippet-edit-cancel")
                            .label(tr!(cx, "取消"))
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cancel_snippet_edit(cx);
                            })),
                    )
                    .child(
                        Button::new("snippet-edit-save")
                            .label(tr!(cx, "保存"))
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.commit_snippet_edit(cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}
