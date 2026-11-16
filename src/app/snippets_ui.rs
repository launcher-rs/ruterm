//! 命令片段：右侧停靠区的列表（搜索 / 插入 / 执行 / 编辑 / 删除）与内联编辑表单。
//!
//! 数据与校验在 `crate::snippets`（含单测）；发送走 `Terminal::input`
//! （Programmatic，不回流同步输入的镜像中间件）。

use rgpui::input_ui::{Input, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, ContextMenuExt, Entity, FontWeight, IconName,
    InteractiveElement, IntoElement, MouseButton, ParentElement, ScrollableElement, SharedString,
    Styled, Tooltip, Window, div, h_flex, px, v_flex,
};

use crate::platform::Platform;
use crate::snippet_presets::{PRESET_GROUPS, PRESETS};
use crate::snippets::CommandSnippet;
use crate::tr;
use crate::{CloseSnippetEditor, ToggleSnippetPanel};

use super::*;

/// 右侧片段区的内联编辑状态；None = 列表视图。
pub(super) struct SnippetEdit {
    /// 正在编辑的片段下标；None = 新建。
    pub(super) index: Option<usize>,
    pub(super) name: Entity<InputState>,
    pub(super) command: Entity<InputState>,
    pub(super) note: Entity<InputState>,
    pub(super) error: Option<SharedString>,
}

impl RuTermApp {
    /// Ctrl+Shift+S：开关右侧片段区；打开时把焦点放到搜索框。
    pub(super) fn toggle_snippet_panel_action(
        &mut self,
        _: &ToggleSnippetPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_snippets_panel(cx);
        if self.snippets_panel_open && self.snippet_edit.is_none() {
            self.snippet_search
                .update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// 片段编辑表单里的 Esc：收起表单回到列表，不影响终端里的 Esc。
    pub(super) fn close_snippet_editor_action(
        &mut self,
        _: &CloseSnippetEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_snippet_edit(window, cx);
    }

    /// 右侧片段区「新建」：展开面板并进入内联表单。
    pub(super) fn new_snippet_from_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_snippet_new(window, cx);
    }

    /// 右侧片段区「编辑」：同样落到内联表单。
    pub(super) fn edit_snippet_from_panel(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_snippet_edit(index, window, cx);
    }

    /// 面板右键删除：菜单本身就是明确动作，不再走二次确认。
    pub(super) fn delete_snippet_from_panel(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.snippets.len() {
            return;
        }
        self.snippets.remove(index);
        self.persist_snippets(cx);
        cx.notify();
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

    /// 导出我的片段为 JSON（默认 ruterm-snippets.json），便于分享或迁移。
    pub(super) fn export_snippets(&mut self, cx: &mut Context<Self>) {
        if self.snippets.is_empty() {
            self.status = tr!(cx, "还没有命令片段可导出");
            cx.notify();
            return;
        }
        let json = crate::snippets::to_json(&self.snippets);
        let start = downloads_dir();
        let entity = cx.entity();
        let rx = cx.prompt_for_new_path(&start, Some("ruterm-snippets.json"));
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let failure = std::fs::write(&path, json).err();
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            entity.update(cx, move |this, cx| {
                this.status = match failure {
                    None => {
                        let path = name.unwrap_or_default();
                        crate::i18n::translate(cx, "已导出命令片段到 {path}", &[("path", &path)])
                    }
                    Some(error) => {
                        let error = error.to_string();
                        crate::i18n::translate(cx, "导出失败：{error}", &[("error", &error)])
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// 从 JSON 导入片段：按「名称 + 命令」去重，已有条目保持不变。
    pub(super) fn import_snippets(&mut self, cx: &mut Context<Self>) {
        let entity = cx.entity();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!(cx, "选择命令片段 JSON 文件")),
        });
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let text = std::fs::read_to_string(&path).map_err(|error| error.to_string());
            entity.update(cx, move |this, cx| {
                let text = match text {
                    Ok(text) => text,
                    Err(error) => {
                        this.status = crate::i18n::translate(
                            cx,
                            "读取片段文件失败：{error}",
                            &[("error", &error)],
                        );
                        cx.notify();
                        return;
                    }
                };
                let imported = crate::snippets::from_json(&text);
                if imported.is_empty() {
                    this.status = tr!(cx, "文件里没有可导入的命令片段");
                } else {
                    let (merged, summary) =
                        crate::snippets::merge_imported(&this.snippets, &imported);
                    this.snippets = merged;
                    this.persist_snippets(cx);
                    let added = summary.added.to_string();
                    let duplicates = summary.duplicates.to_string();
                    let over_limit = summary.over_limit.to_string();
                    let args: &[(&str, &str)] = &[
                        ("added", &added),
                        ("duplicates", &duplicates),
                        ("over_limit", &over_limit),
                    ];
                    this.status = if summary.over_limit > 0 {
                        crate::i18n::translate(
                            cx,
                            "已导入 {added} 条命令片段，跳过重复 {duplicates} 条、超限 {over_limit} 条",
                            args,
                        )
                    } else {
                        crate::i18n::translate(
                            cx,
                            "已导入 {added} 条命令片段，跳过重复 {duplicates} 条",
                            args,
                        )
                    };
                }
                cx.notify();
            });
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
        self.snippets_panel_open = true;
        self.snippet_panel_builtin = false;
        let edit = Self::make_snippet_edit(index, name, command, note, window, cx);
        let name_input = edit.name.clone();
        self.snippet_edit = Some(edit);
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

    /// 把内置命令复制进「我的片段」编辑器；取消即回列表。
    pub(super) fn copy_preset_to_snippets(
        &mut self,
        preset_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(preset) = crate::snippet_presets::PRESETS.get(preset_index) else {
            return;
        };
        self.begin_snippet_edit_values(None, preset.name, preset.command, preset.note, window, cx);
    }

    pub(super) fn cancel_snippet_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_edit.take().is_some() {
            // 表单销毁后把焦点交回搜索框，接着敲键盘还能继续筛选列表。
            self.snippet_search
                .update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }

    fn set_snippet_edit_error(&mut self, reason: &str, cx: &mut Context<Self>) {
        if let Some(edit) = self.snippet_edit.as_mut() {
            edit.error = Some(crate::i18n::translate(cx, reason, &[]));
        }
        cx.notify();
    }

    /// 保存表单：校验 → 原位替换或追加 → 持久化。
    pub(super) fn commit_snippet_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.snippet_edit.as_ref() else {
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
        self.snippet_edit = None;
        self.snippet_search
            .update(cx, |input, cx| input.focus(window, cx));
        self.persist_snippets(cx);
        self.status =
            crate::i18n::translate(cx, "已保存命令片段：{name}", &[("name", &snippet.name)]);
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
            .overflow_y_scrollbar()
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
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_snippet_edit(window, cx);
                            })),
                    )
                    .child(
                        Button::new("snippet-edit-save")
                            .label(tr!(cx, "保存"))
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.commit_snippet_edit(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// 右侧停靠区的片段分区：我的片段 / 内置命令两页 + 搜索 + 紧凑列表 + 底栏。
    pub(super) fn render_snippets_section(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        if let Some(edit) = self.snippet_edit.as_ref() {
            return self.render_snippet_editor_panel(width, edit, &pal, cx);
        }
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
                    || preset
                        .fallback()
                        .is_some_and(|text| text.to_lowercase().contains(&query))
            };
            let mut rows: Vec<usize> = PRESETS
                .iter()
                .enumerate()
                .filter(|(_, preset)| preset.supports(viewing) && matches(preset))
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
                    "还没有命令片段。点右上角 + 新建一条，或切到「内置命令」。"
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
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(pal.muted)
                            .text_ellipsis()
                            .child(if builtin {
                                tr!(cx, "只读 · 单击插入 · ▶ 执行 · 右键存为我的片段")
                            } else {
                                tr!(cx, "单击插入 · ▶ 执行 · 右键更多")
                            }),
                    )
                    .when(!builtin, |row| {
                        row.child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    Button::new("snippet-panel-import")
                                        .label(tr!(cx, "导入…"))
                                        .ghost()
                                        .compact()
                                        .tooltip(tr!(cx, "从 JSON 导入，按名称与命令去重"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.import_snippets(cx);
                                        })),
                                )
                                .child(
                                    Button::new("snippet-panel-export")
                                        .label(tr!(cx, "导出…"))
                                        .ghost()
                                        .compact()
                                        .tooltip(tr!(cx, "导出全部我的片段为 JSON"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.export_snippets(cx);
                                        })),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    /// 内联编辑表单：与列表共用同一块右侧面板。
    fn render_snippet_editor_panel(
        &self,
        width: f32,
        edit: &SnippetEdit,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = if edit.index.is_some() {
            tr!(cx, "编辑片段")
        } else {
            tr!(cx, "新建片段")
        };
        v_flex()
            .id("snippet-panel")
            .key_context("SnippetEditor")
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
                            .child(title),
                    )
                    .child(
                        Button::new("snippet-panel-editor-close")
                            .icon(IconName::Close)
                            .ghost()
                            .compact()
                            .tooltip(tr!(cx, "关闭片段面板"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_snippets_panel(cx);
                            })),
                    ),
            )
            .child(self.render_snippet_editor(edit, pal, cx))
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
        let mut tip = format!("{}\n{}", preset.command, note);
        if let Some(fallback) = preset.fallback() {
            tip.push('\n');
            tip.push_str(&crate::i18n::translate(cx, fallback, &[]));
        }
        let tip: SharedString = tip.into();
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
