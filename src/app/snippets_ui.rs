//! 命令片段管理器：列表（搜索 / 插入 / 执行 / 编辑 / 删除）+ 编辑表单。
//!
//! 数据与校验在 `crate::snippets`（含单测）；发送走 `Terminal::input`
//! （Programmatic，不回流同步输入的镜像中间件）。

use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, ContextMenuExt, Disableable, Entity, FontWeight,
    IconName, InteractiveElement, IntoElement, MouseButton, ParentElement, ScrollableElement,
    SharedString, Styled, Tooltip, Window, div, h_flex, px, v_flex,
};

use crate::platform::Platform;
use crate::snippet_presets::{PRESET_GROUPS, PRESETS};
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
    /// 内置命令页正在查看的平台（打开时默认为当前会话平台，可手动切换）。
    pub(super) preset_platform: Platform,
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
    pub(super) fn open_snippet_manager_impl(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            preset_platform: self.active_session_platform().unwrap_or(Platform::Cross),
        });
        cx.notify();
    }

    /// 右侧片段区「新建」：复用管理器的编辑表单，管理器还没开就先开。
    pub(super) fn new_snippet_from_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_manager.is_none() {
            self.open_snippet_manager_impl(window, cx);
        }
        self.snippet_panel_builtin = false;
        self.begin_snippet_new(window, cx);
    }

    /// 右侧片段区「编辑」：同样落到管理器的编辑表单。
    pub(super) fn edit_snippet_from_panel(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_manager.is_none() {
            self.open_snippet_manager_impl(window, cx);
        }
        self.snippet_panel_builtin = false;
        self.begin_snippet_edit(index, window, cx);
    }

    /// 面板右键删除：菜单本身就是明确动作，不再走弹窗的两步确认。
    pub(super) fn delete_snippet_from_panel(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.snippets.len() {
            return;
        }
        self.snippets.remove(index);
        if let Some(manager) = self.snippet_manager.as_mut() {
            manager.confirm_delete = None;
        }
        self.persist_snippets(cx);
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

    /// 切换内置命令页查看的平台。
    pub(super) fn set_preset_platform(&mut self, platform: Platform, cx: &mut Context<Self>) {
        if let Some(manager) = self.snippet_manager.as_mut()
            && manager.preset_platform != platform
        {
            manager.preset_platform = platform;
            cx.notify();
        }
    }

    /// 右侧片段分区切换「我的片段 / 内置命令」。
    pub(super) fn set_snippet_panel_tab(&mut self, builtin: bool, cx: &mut Context<Self>) {
        if self.snippet_panel_builtin != builtin {
            self.snippet_panel_builtin = builtin;
            cx.notify();
        }
    }

    /// 右侧片段分区内置页切换平台。
    pub(super) fn set_snippet_panel_platform(
        &mut self,
        platform: Platform,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_panel_platform != platform {
            self.snippet_panel_platform = platform;
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
        if self.snippet_manager.is_none() {
            self.open_snippet_manager_impl(window, cx);
        }
        self.begin_snippet_edit_values(None, preset.name, preset.command, preset.note, window, cx);
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
        self.status =
            crate::i18n::translate(cx, "已保存命令片段：{name}", &[("name", &snippet.name)]);
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
        let key = if run {
            "已执行片段：{name}"
        } else {
            "已插入片段：{name}"
        };
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
        let key = if run {
            "已执行内置命令：{name}"
        } else {
            "已插入内置命令：{name}"
        };
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
                                    .child(div().text_xs().text_color(pal.muted).child(tr!(
                                        cx,
                                        "保存常用命令，按需插入或直接执行到当前会话。"
                                    ))),
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
        // 默认展示当前会话平台，可用下方平台按钮切到其它平台查看；平台内按子分类分组。
        let active = self.active_session_platform();
        let viewing = state.preset_platform;
        let mut rows: Vec<usize> = PRESETS
            .iter()
            .enumerate()
            .filter(|(_, preset)| preset.platform() == viewing && matches(preset))
            .map(|(index, _)| index)
            .collect();
        rows.sort_by_key(|index| {
            PRESET_GROUPS
                .iter()
                .position(|group| *group == PRESETS[*index].group)
                .unwrap_or(usize::MAX)
        });
        let mut list = v_flex().id("preset-list").gap_1().overflow_y_scrollbar();
        if !rows.is_empty() {
            let mut heading = crate::i18n::translate(cx, viewing.label(), &[]).to_string();
            if active == Some(viewing) {
                heading.push_str(&crate::i18n::translate(cx, "（当前会话）", &[]));
            }
            list = list.child(
                div()
                    .pt_2()
                    .pb_1()
                    .text_sm()
                    .font_weight(FontWeight(600.0))
                    .text_color(pal.text)
                    .child(SharedString::from(heading)),
            );
            let mut group = None;
            for index in rows {
                let preset = &PRESETS[index];
                if group != Some(preset.group) {
                    group = Some(preset.group);
                    list = list.child(
                        div()
                            .pl_2()
                            .pt_1()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(crate::i18n::translate(cx, preset.group, &[])),
                    );
                }
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
        } else {
            list = list.child(
                div()
                    .py_4()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "没有匹配的内置命令")),
            );
        }
        let mut platform_row = h_flex().gap_1().items_center().child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(pal.muted)
                .child(tr!(cx, "平台")),
        );
        for platform in Platform::ALL {
            let selected = platform == viewing;
            let mut chip = Button::new(("preset-platform", platform as usize))
                .label(crate::i18n::translate(cx, platform.label(), &[]))
                .compact()
                .when(selected, |button| button.primary())
                .when(!selected, |button| button.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_preset_platform(platform, cx);
                }));
            if active == Some(platform) {
                chip = chip.tooltip(tr!(cx, "当前会话平台"));
            }
            platform_row = platform_row.child(chip);
        }
        v_flex()
            .gap_3()
            .flex_1()
            .min_h_0()
            .child(styled_input(Input::new(&state.search), pal))
            .child(platform_row)
            .child(list)
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(
                        cx,
                        "内置命令只读；可切换平台查看，补全只提示当前会话平台的命令。需要固定参数时点「存为我的片段」再改。"
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
                            .child(div().text_sm().text_color(pal.text).child(
                                if snippet.has_name() {
                                    snippet.name.clone().into()
                                } else {
                                    tr!(cx, "未命名片段")
                                },
                            ))
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
                    .child(
                        div()
                            .flex_1()
                            .child(styled_input(Input::new(&state.search), pal)),
                    )
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
                    .child(tr!(cx, "片段保存在加密库；「执行」会自动追加回车。")),
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
                    .child(tr!(cx, "单行命令；内容按字面发送，不做变量替换。")),
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

    /// 右侧停靠区的片段分区：我的片段 / 内置命令两页 + 搜索 + 紧凑列表 + 底栏。
    pub(super) fn render_snippets_section(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let query = self.snippet_search.read(cx).value().trim().to_lowercase();
        let can_send = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .is_some_and(|tab| !tab.ended && tab.terminal_view.is_some());
        let builtin = self.snippet_panel_builtin;
        let active = self.active_session_platform();
        let viewing = self.snippet_panel_platform;

        let mut list = v_flex()
            .id("snippet-panel-list")
            .flex_1()
            .min_h_0()
            .gap_px()
            .overflow_y_scrollbar();
        let mut shown = 0;
        if builtin {
            let matches = |preset: &crate::snippet_presets::PresetSnippet| {
                query.is_empty()
                    || preset.name.to_lowercase().contains(&query)
                    || preset.command.to_lowercase().contains(&query)
                    || preset.note.to_lowercase().contains(&query)
            };
            let mut rows: Vec<usize> = PRESETS
                .iter()
                .enumerate()
                .filter(|(_, preset)| preset.platform() == viewing && matches(preset))
                .map(|(index, _)| index)
                .collect();
            rows.sort_by_key(|index| {
                PRESET_GROUPS
                    .iter()
                    .position(|group| *group == PRESETS[*index].group)
                    .unwrap_or(usize::MAX)
            });
            let mut group = None;
            for index in rows {
                let preset = &PRESETS[index];
                if group != Some(preset.group) {
                    group = Some(preset.group);
                    list = list.child(
                        div()
                            .pl_2()
                            .pt_1()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(crate::i18n::translate(cx, preset.group, &[])),
                    );
                }
                shown += 1;
                list = list.child(self.render_preset_panel_row(index, preset, &pal, can_send, cx));
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
        } else {
            if self.snippets.is_empty() {
                list = list.child(div().py_6().text_sm().text_color(pal.muted).child(tr!(
                    cx,
                    "还没有命令片段。点「新建片段」添加一条，或切到「内置命令」。"
                )));
            }
            for (index, snippet) in self.snippets.iter().enumerate() {
                if !query.is_empty()
                    && !snippet.name.to_lowercase().contains(&query)
                    && !snippet.command.to_lowercase().contains(&query)
                    && !snippet.note.to_lowercase().contains(&query)
                {
                    continue;
                }
                shown += 1;
                list =
                    list.child(self.render_snippet_panel_row(index, snippet, &pal, can_send, cx));
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
        }

        let tab = |id: &'static str, label: SharedString, selected: bool, target: bool| {
            Button::new(id)
                .label(label)
                .compact()
                .when(selected, |button| button.primary())
                .when(!selected, |button| button.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_snippet_panel_tab(target, cx);
                }))
        };
        let tabs = h_flex()
            .w_full()
            .gap_1()
            .child(tab(
                "snippet-panel-tab-mine",
                tr!(cx, "我的片段"),
                !builtin,
                false,
            ))
            .child(tab(
                "snippet-panel-tab-builtin",
                tr!(cx, "内置命令"),
                builtin,
                true,
            ));
        let mut platform_row = h_flex().gap_1().items_center().child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(pal.muted)
                .child(tr!(cx, "平台")),
        );
        for platform in Platform::ALL {
            let selected = platform == viewing;
            let mut chip = Button::new(("snippet-panel-platform", platform as usize))
                .label(crate::i18n::translate(cx, platform.label(), &[]))
                .compact()
                .when(selected, |button| button.primary())
                .when(!selected, |button| button.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_snippet_panel_platform(platform, cx);
                }));
            if active == Some(platform) {
                chip = chip.tooltip(tr!(cx, "当前会话平台"));
            }
            platform_row = platform_row.child(chip);
        }

        v_flex()
            .id("snippet-panel")
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
                            .child(tr!(cx, "命令片段")),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Button::new("snippet-panel-new")
                                    .icon(IconName::Plus)
                                    .ghost()
                                    .compact()
                                    .tooltip(tr!(cx, "新建片段"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_snippet_from_panel(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("snippet-panel-close")
                                    .icon(IconName::Close)
                                    .ghost()
                                    .compact()
                                    .tooltip(tr!(cx, "关闭片段面板"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_snippets_panel(cx);
                                    })),
                            ),
                    ),
            )
            .child(tabs)
            .child(styled_input(Input::new(&self.snippet_search), &pal))
            .when(builtin, |this| this.child(platform_row))
            .child(list)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().text_color(pal.muted).text_ellipsis().child(
                        if builtin {
                            tr!(cx, "只读 · 单击插入 · ▶ 执行 · 右键存为我的片段")
                        } else {
                            tr!(cx, "单击插入 · ▶ 执行 · 右键更多")
                        },
                    ))
                    .child(
                        Button::new("snippet-panel-manage")
                            .label(tr!(cx, "管理…"))
                            .ghost()
                            .compact()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_snippet_manager_impl(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// 右侧内置命令行：单击插入、▶ 执行、右键存为我的片段。
    fn render_preset_panel_row(
        &self,
        index: usize,
        preset: &crate::snippet_presets::PresetSnippet,
        pal: &Palette,
        can_send: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let menu_entity = cx.entity();
        let note = crate::i18n::translate(cx, preset.note, &[]);
        let tip: SharedString = format!("{}\n{}", preset.command, note).into();
        h_flex()
            .id(("preset-panel-row", index))
            .w_full()
            .h(px(28.0))
            .items_center()
            .gap_2()
            .pl_2()
            .pr_1()
            .rounded(px(4.0))
            .hover(|style| style.bg(pal.card_hover))
            .cursor_pointer()
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.send_preset(index, false, cx);
            }))
            .context_menu(move |menu, _, cx| {
                let insert = menu_entity.clone();
                let run = menu_entity.clone();
                let copy = menu_entity.clone();
                menu.min_w(px(168.0))
                    .item(Self::menu_item(
                        insert,
                        tr!(cx, "插入到当前会话"),
                        None,
                        false,
                        false,
                        move |this, _, cx| {
                            this.send_preset(index, false, cx);
                        },
                    ))
                    .item(
                        Self::menu_item(
                            run,
                            tr!(cx, "执行（回车）"),
                            Some(IconName::Play),
                            false,
                            false,
                            move |this, _, cx| {
                                this.send_preset(index, true, cx);
                            },
                        )
                        .disabled(!can_send),
                    )
                    .separator()
                    .item(Self::menu_item(
                        copy,
                        tr!(cx, "存为我的片段"),
                        None,
                        false,
                        false,
                        move |this, window, cx| {
                            this.copy_preset_to_snippets(index, window, cx);
                        },
                    ))
            })
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px(110.0))
                    .text_sm()
                    .text_color(pal.text)
                    .text_ellipsis()
                    .child(crate::i18n::translate(cx, preset.name, &[])),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(pal.muted)
                    .text_ellipsis()
                    .child(preset.command),
            )
            .when(can_send, |row| {
                row.child(
                    Button::new(("preset-panel-run", index))
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
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send_preset(index, true, cx);
                        })),
                )
            })
            .into_any_element()
    }

    fn render_snippet_panel_row(
        &self,
        index: usize,
        snippet: &CommandSnippet,
        pal: &Palette,
        can_send: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let menu_entity = cx.entity();
        let name: SharedString = if snippet.has_name() {
            snippet.name.clone().into()
        } else {
            tr!(cx, "未命名片段")
        };
        let tip: SharedString = if snippet.note.is_empty() {
            snippet.command.clone().into()
        } else {
            format!("{}\n{}", snippet.command, snippet.note).into()
        };
        h_flex()
            .id(("snippet-panel-row", index))
            .w_full()
            .h(px(28.0))
            .items_center()
            .gap_2()
            .pl_2()
            .pr_1()
            .rounded(px(4.0))
            .hover(|style| style.bg(pal.card_hover))
            .cursor_pointer()
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.send_snippet(index, false, cx);
            }))
            .context_menu(move |menu, _, cx| {
                let insert = menu_entity.clone();
                let run = menu_entity.clone();
                let edit = menu_entity.clone();
                let delete = menu_entity.clone();
                menu.min_w(px(168.0))
                    .item(Self::menu_item(
                        insert,
                        tr!(cx, "插入到当前会话"),
                        None,
                        false,
                        false,
                        move |this, _, cx| {
                            this.send_snippet(index, false, cx);
                        },
                    ))
                    .item(
                        Self::menu_item(
                            run,
                            tr!(cx, "执行（回车）"),
                            Some(IconName::Play),
                            false,
                            false,
                            move |this, _, cx| {
                                this.send_snippet(index, true, cx);
                            },
                        )
                        .disabled(!can_send),
                    )
                    .separator()
                    .item(Self::menu_item(
                        edit,
                        tr!(cx, "编辑"),
                        None,
                        false,
                        false,
                        move |this, window, cx| {
                            this.edit_snippet_from_panel(index, window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        delete,
                        tr!(cx, "删除"),
                        Some(IconName::Delete),
                        true,
                        false,
                        move |this, _, cx| {
                            this.delete_snippet_from_panel(index, cx);
                        },
                    ))
            })
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px(110.0))
                    .text_sm()
                    .text_color(pal.text)
                    .text_ellipsis()
                    .child(name),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(pal.muted)
                    .text_ellipsis()
                    .child(SharedString::from(snippet.command.clone())),
            )
            .when(can_send, |row| {
                row.child(
                    Button::new(("snippet-panel-run", index))
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
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send_snippet(index, true, cx);
                        })),
                )
            })
            .into_any_element()
    }
}
