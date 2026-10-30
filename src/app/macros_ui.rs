//! 键宏管理器：列表（搜索 / 启停 / ▶执行 / 编辑 / 删除）+ 内联编辑表单
//! （名称 / 触发键 / 步骤 + 每步延迟）+ 导入导出。
//!
//! 数据与校验在 `crate::macros`（含单测）；这里只管状态与界面。
//! 触发拦截走 `App::intercept_keystrokes`（在所有动作分发之前），执行复用命令
//! 广播的写入通道（`Terminal::input`，Programmatic 来源不回流镜像中间件）。

use std::time::Duration;

use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, Disableable, Entity, FontWeight, InteractiveElement,
    IntoElement, ParentElement, ScrollableElement, SharedString, Styled, Window, div, h_flex, px,
    v_flex,
};

use crate::macros::{KeyMacro, MacroStep};
use crate::tr;
use crate::{CloseMacroManager, MacroManager as OpenMacroManager};

use super::*;

/// 键宏管理器弹窗状态：列表 ⇄ 编辑表单二选一。
pub(super) struct MacroManager {
    pub(super) search: Entity<InputState>,
    /// None = 列表视图；Some = 正在新建 / 编辑。
    pub(super) editing: Option<MacroEdit>,
    /// 待确认删除的下标（再点一次「删除」才真删）。
    pub(super) confirm_delete: Option<usize>,
}

/// 编辑表单：字段各自一个输入框，步骤可增删。
pub(super) struct MacroEdit {
    /// 正在编辑的宏下标；None = 新建。
    pub(super) index: Option<usize>,
    pub(super) name: Entity<InputState>,
    pub(super) trigger: Entity<InputState>,
    pub(super) error: Option<SharedString>,
    pub(super) steps: Vec<MacroStepEdit>,
}

pub(super) struct MacroStepEdit {
    pub(super) text: Entity<InputState>,
    pub(super) delay: Entity<InputState>,
}

impl RuTermApp {
    /// Ctrl+Shift+M /「会话」菜单：打开键宏管理器。
    pub(super) fn open_macro_manager(
        &mut self,
        _: &OpenMacroManager,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.macro_manager.as_ref() {
            state.search.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
            return;
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "搜索键宏…")));
        cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        search.update(cx, |input, cx| input.focus(window, cx));
        self.macro_manager = Some(MacroManager {
            search,
            editing: None,
            confirm_delete: None,
        });
        cx.notify();
    }

    pub(super) fn close_macro_manager_action(
        &mut self,
        _: &CloseMacroManager,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_macro_manager(window, cx);
    }

    pub(super) fn close_macro_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.macro_manager.take().is_none() {
            return;
        }
        if let Some(view) = self.active_terminal_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// 键宏持久化：加密库 `settings.macros`，库不可用时降级 `settings.json`。
    pub(super) fn persist_macros(&self, cx: &mut Context<Self>) {
        let json = crate::macros::to_json(&self.macros);
        let Some(vault) = self.vault.clone() else {
            self.store.save_macros(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("macros", &json).await {
                log::warn!("写入库内键宏失败: {error}");
            }
        })
        .detach();
    }

    /// 全局按键拦截（`intercept_keystrokes`）：命中启用的键宏时吞掉按键并执行。
    pub(super) fn intercept_macro_key(
        &mut self,
        event: &rgpui::KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.macros.is_empty() || self.overlay_open() {
            return;
        }
        // 只认终端真正持有焦点的时候：表单 / 弹窗输入框里敲到的同名组合不该执行宏。
        let Some(view) = self.active_terminal_view() else {
            return;
        };
        if !view.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        let Some(index) = self
            .macros
            .iter()
            .position(|mac| mac.matches_keystroke(&event.keystroke))
        else {
            return;
        };
        cx.stop_propagation();
        self.run_macro(index, cx);
    }

    /// 执行一条键宏：逐步骤写给活动终端，步间按延迟等待；计入使用次数。
    ///
    /// 写入走 `Terminal::input`（Programmatic），与同步输入共用同一条通道，
    /// 不会经镜像中间件回流到其它终端。
    pub(super) fn run_macro(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(terminal) = self
            .active_terminal_view()
            .map(|view| view.read(cx).terminal().clone())
        else {
            self.status = tr!(cx, "没有可执行键宏的活动会话");
            cx.notify();
            return;
        };
        let Some(mac) = self.macros.get_mut(index) else {
            return;
        };
        mac.uses += 1;
        let mac = mac.clone();
        let name = if mac.name.is_empty() {
            tr!(cx, "未命名宏").to_string()
        } else {
            mac.name.clone()
        };
        self.status = crate::i18n::translate(cx, "已执行键宏：{name}", &[("name", &name)]);
        self.persist_macros(cx);
        cx.spawn(async move |_, cx| {
            for step in &mac.steps {
                let bytes = step.to_bytes();
                if !bytes.is_empty() {
                    terminal.update(cx, |terminal, _| terminal.input(bytes));
                }
                if step.delay_ms > 0 {
                    cx.background_executor()
                        .timer(Duration::from_millis(step.delay_ms))
                        .await;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// 列表行开关：禁用后触发键不再拦截（仍可从列表手动执行）。
    pub(super) fn toggle_macro_enabled(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(mac) = self.macros.get_mut(index) {
            mac.enabled = !mac.enabled;
        }
        self.persist_macros(cx);
        cx.notify();
    }

    /// 删除（两步确认：第一次只把按钮换成「确认删除」）。
    pub(super) fn request_delete_macro(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(manager) = self.macro_manager.as_mut() else {
            return;
        };
        if manager.confirm_delete != Some(index) {
            manager.confirm_delete = Some(index);
            cx.notify();
            return;
        }
        manager.confirm_delete = None;
        if index < self.macros.len() {
            self.macros.remove(index);
            self.persist_macros(cx);
        }
        cx.notify();
    }

    /// 新建 / 编辑共用的表单构造。
    fn make_macro_edit(
        index: Option<usize>,
        name: &str,
        trigger: &str,
        steps: &[MacroStep],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> MacroEdit {
        let name_input = cx.new(|cx| InputState::new(window, cx).default_value(name.to_string()));
        let trigger_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(trigger.to_string())
                .placeholder(tr!(cx, "留空 = 不绑键，例：ctrl-alt-1"))
        });
        let mut step_edits = Vec::new();
        for step in steps {
            step_edits.push(Self::make_step_edit(&step.text, step.delay_ms, window, cx));
        }
        if step_edits.is_empty() {
            step_edits.push(Self::make_step_edit("", 0, window, cx));
        }
        MacroEdit {
            index,
            name: name_input,
            trigger: trigger_input,
            error: None,
            steps: step_edits,
        }
    }

    fn make_step_edit(
        text: &str,
        delay_ms: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> MacroStepEdit {
        let text_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(text.to_string())
                .placeholder(tr!(cx, "发送内容（\\n 回车 · \\t Tab · \\e ESC）"))
        });
        let delay = if delay_ms == 0 {
            String::new()
        } else {
            delay_ms.to_string()
        };
        let delay_input =
            cx.new(|cx| InputState::new(window, cx).default_value(delay).placeholder(tr!(cx, "0")));
        MacroStepEdit {
            text: text_input,
            delay: delay_input,
        }
    }

    pub(super) fn begin_macro_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.macro_manager.as_ref().is_none() {
            return;
        }
        let edit = Self::make_macro_edit(None, "", "", &[], window, cx);
        let name = edit.name.clone();
        if let Some(manager) = self.macro_manager.as_mut() {
            manager.editing = Some(edit);
            manager.confirm_delete = None;
        }
        name.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn begin_macro_edit(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mac) = self.macros.get(index).cloned() else {
            return;
        };
        let edit = Self::make_macro_edit(
            Some(index),
            &mac.name,
            mac.trigger.as_deref().unwrap_or_default(),
            &mac.steps,
            window,
            cx,
        );
        let name = edit.name.clone();
        if let Some(manager) = self.macro_manager.as_mut() {
            manager.editing = Some(edit);
            manager.confirm_delete = None;
        }
        name.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn add_macro_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = Self::make_step_edit("", 0, window, cx);
        if let Some(manager) = self.macro_manager.as_mut()
            && let Some(edit) = manager.editing.as_mut()
        {
            edit.steps.push(row);
        }
        cx.notify();
    }

    pub(super) fn remove_macro_step(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(manager) = self.macro_manager.as_mut()
            && let Some(edit) = manager.editing.as_mut()
            && edit.steps.len() > 1
            && index < edit.steps.len()
        {
            edit.steps.remove(index);
        }
        cx.notify();
    }

    pub(super) fn cancel_macro_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(manager) = self.macro_manager.as_mut() {
            manager.editing = None;
            manager.confirm_delete = None;
        }
        cx.notify();
    }

    fn set_macro_edit_error(&mut self, reason: String, cx: &mut Context<Self>) {
        if let Some(manager) = self.macro_manager.as_mut()
            && let Some(edit) = manager.editing.as_mut()
        {
            edit.error = Some(SharedString::from(reason));
        }
        cx.notify();
    }

    /// 保存表单：触发键校验（文法 / 安全性 / 去重）→ 写回列表并持久化。
    pub(super) fn commit_macro_edit(&mut self, cx: &mut Context<Self>) {
        let Some(manager) = self.macro_manager.as_ref() else {
            return;
        };
        let Some(edit) = manager.editing.as_ref() else {
            return;
        };
        let trigger_raw = edit.trigger.read(cx).value().to_string();
        let trigger = match crate::macros::normalize_trigger(&trigger_raw) {
            Ok(trigger) => trigger,
            Err(reason) => {
                self.set_macro_edit_error(reason, cx);
                return;
            }
        };
        // 同一组合只能绑一条：先保存的赢，避免按键归属含糊。
        if let Some(trigger) = trigger.as_ref()
            && self
                .macros
                .iter()
                .enumerate()
                .any(|(ix, mac)| Some(ix) != edit.index && mac.trigger.as_deref() == Some(trigger))
        {
            self.set_macro_edit_error("该触发键已被其它键宏占用".into(), cx);
            return;
        }
        // 名字留空就存空串：显示时再回落「未命名宏」，不把界面语言写进数据。
        let name = edit.name.read(cx).value().trim().to_string();
        let mut steps = Vec::new();
        for row in &edit.steps {
            let text = row.text.read(cx).value().to_string();
            let delay_ms = row.delay.read(cx).value().trim().parse::<u64>().unwrap_or(0);
            steps.push(MacroStep { text, delay_ms });
        }
        if steps.iter().all(|step| step.text.is_empty() && step.delay_ms == 0) {
            self.set_macro_edit_error("至少要填一步发送内容".into(), cx);
            return;
        }
        let index = edit.index;
        match index {
            Some(ix) if ix < self.macros.len() => {
                let uses = self.macros[ix].uses;
                let enabled = self.macros[ix].enabled;
                self.macros[ix] = KeyMacro {
                    name: name.clone(),
                    trigger,
                    enabled,
                    uses,
                    steps,
                };
            }
            _ => self.macros.push(KeyMacro {
                name: name.clone(),
                trigger,
                enabled: true,
                uses: 0,
                steps,
            }),
        }
        if let Some(manager) = self.macro_manager.as_mut() {
            manager.editing = None;
        }
        self.persist_macros(cx);
        let shown = if name.is_empty() {
            tr!(cx, "未命名宏").to_string()
        } else {
            name
        };
        self.status = crate::i18n::translate(cx, "已保存键宏：{name}", &[("name", &shown)]);
        cx.notify();
    }

    /// 导出：选保存位置后写 JSON（默认文件名 ruterm-macros.json）。
    pub(super) fn export_macros(&mut self, cx: &mut Context<Self>) {
        if self.macros.is_empty() {
            self.status = tr!(cx, "还没有键宏可导出");
            cx.notify();
            return;
        }
        let json = crate::macros::to_json(&self.macros);
        let start = downloads_dir();
        let entity = cx.entity();
        let rx = cx.prompt_for_new_path(&start, Some("ruterm-macros.json"));
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
                        crate::i18n::translate(cx, "已导出键宏到 {path}", &[("path", &path)])
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

    /// 导入：选一个 JSON 文件，整体追加进现有列表（同名不覆盖）。
    pub(super) fn import_macros(&mut self, cx: &mut Context<Self>) {
        let entity = cx.entity();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!(cx, "选择键宏 JSON 文件")),
        });
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let imported = std::fs::read_to_string(&path)
                .map(|text| crate::macros::from_json(&text))
                .unwrap_or_default();
            let count = imported.len();
            entity.update(cx, move |this, cx| {
                if imported.is_empty() {
                    this.status = tr!(cx, "文件里没有可导入的键宏");
                } else {
                    this.macros.extend(imported);
                    this.persist_macros(cx);
                    this.status = crate::i18n::translate(
                        cx,
                        "已导入 {count} 条键宏",
                        &[("count", &count.to_string())],
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 管理器弹窗：列表与编辑表单共用同一张卡片。
    pub(super) fn render_macro_manager(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.macro_manager.as_ref() else {
            return div().into_any_element();
        };
        let body = match state.editing.as_ref() {
            Some(edit) => self.render_macro_editor(edit, &pal, cx),
            None => self.render_macro_list(state, &pal, cx),
        };
        v_flex()
            .id("macro-manager-backdrop")
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
                    .id("macro-manager")
                    .key_context("MacroManager")
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
                                            .child(tr!(cx, "键宏")),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(pal.muted)
                                            .child(tr!(
                                                cx,
                                                "组合键触发一段固定输入；发送走当前活动会话的写入通道。"
                                            )),
                                    ),
                            )
                            .child(Button::new("close-macro-manager").label("×").ghost().on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.close_macro_manager(window, cx);
                                }),
                            )),
                    )
                    .child(body),
            )
            .into_any_element()
    }

    /// 列表视图：搜索 + 每行（开关 / 名称 / 触发键 / 步数 / 次数 / 执行 / 编辑 / 删除）。
    fn render_macro_list(
        &self,
        state: &MacroManager,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let query = state.search.read(cx).value().trim().to_lowercase();
        let matches = |mac: &KeyMacro| {
            query.is_empty()
                || mac.name.to_lowercase().contains(&query)
                || mac
                    .trigger
                    .as_deref()
                    .map(|trigger| trigger.to_lowercase().contains(&query))
                    .unwrap_or(false)
        };
        let mut list = v_flex().id("macro-list").gap_1().overflow_y_scrollbar();
        if self.macros.is_empty() {
            list = list.child(
                div()
                    .py_6()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(
                        cx,
                        "还没有键宏。点「新建宏」录入一条，或从 JSON 文件导入。"
                    )),
            );
        }
        let mut shown = 0;
        for (index, mac) in self.macros.iter().enumerate() {
            if !matches(mac) {
                continue;
            }
            shown += 1;
            let confirming = state.confirm_delete == Some(index);
            let trigger_label = match mac.trigger.as_deref() {
                Some(trigger) => tr!(cx, "触发 {trigger}", trigger = trigger),
                None => tr!(cx, "未绑触发键"),
            };
            let steps_text = mac.step_count().to_string();
            let uses_text = mac.uses.to_string();
            let meta = crate::i18n::translate(
                cx,
                "{trigger_label} · {steps} 步 · 用 {uses} 次",
                &[
                    ("trigger_label", trigger_label.as_ref()),
                    ("steps", &steps_text),
                    ("uses", &uses_text),
                ],
            );
            list = list.child(
                h_flex()
                    .id(("macro-row", index))
                    .w_full()
                    .items_center()
                    .gap_3()
                    .p_2()
                    .rounded(px(5.0))
                    .hover(|style| style.bg(pal.card_hover))
                    .child(forward_switch(
                        format!("macro-switch-{index}"),
                        mac.enabled,
                        pal,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_macro_enabled(index, cx);
                        }),
                    ))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if mac.enabled { pal.text } else { pal.muted })
                                    .child(if mac.name.is_empty() {
                                        tr!(cx, "未命名宏")
                                    } else {
                                        mac.name.clone().into()
                                    }),
                            )
                            .child(div().text_xs().text_color(pal.muted).child(meta)),
                    )
                    .child(
                        Button::new(("macro-run", index))
                            .label("▶")
                            .ghost()
                            .compact()
                            .tooltip(tr!(cx, "在当前活动会话执行"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_macro(index, cx);
                            })),
                    )
                    .child(
                        Button::new(("macro-edit", index))
                            .label(tr!(cx, "编辑"))
                            .ghost()
                            .compact()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.begin_macro_edit(index, window, cx);
                            })),
                    )
                    .child(
                        Button::new(("macro-delete", index))
                            .label(if confirming {
                                tr!(cx, "确认删除")
                            } else {
                                tr!(cx, "删除")
                            })
                            .ghost()
                            .compact()
                            .when(confirming, |button| button.text_color(pal.danger))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.request_delete_macro(index, cx);
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
                    .child(tr!(cx, "没有匹配的键宏")),
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
                        Button::new("macro-new")
                            .label(tr!(cx, "新建宏"))
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.begin_macro_new(window, cx);
                            })),
                    ),
            )
            .child(list)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(tr!(
                                cx,
                                "触发键需带 Ctrl / Alt（或 F1–F12）；终端未聚焦时不会触发。"
                            )),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("macro-import")
                                    .label(tr!(cx, "导入…"))
                                    .ghost()
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.import_macros(cx);
                                    })),
                            )
                            .child(
                                Button::new("macro-export")
                                    .label(tr!(cx, "导出…"))
                                    .ghost()
                                    .compact()
                                    .disabled(self.macros.is_empty())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.export_macros(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 编辑视图：名称 / 触发键 / 步骤序列（文本 + 延迟）/ 错误提示 / 保存取消。
    fn render_macro_editor(
        &self,
        edit: &MacroEdit,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut steps = v_flex().id("macro-steps").gap_2().overflow_y_scrollbar();
        for (index, row) in edit.steps.iter().enumerate() {
            let removable = edit.steps.len() > 1;
            steps = steps.child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(styled_input(Input::new(&row.text), pal)),
                    )
                    .child(
                        div()
                            .w(px(76.0))
                            .flex_shrink_0()
                            .child(styled_input(Input::new(&row.delay), pal)),
                    )
                    .child(
                        Button::new(("macro-step-remove", index))
                            .label("×")
                            .ghost()
                            .compact()
                            .disabled(!removable)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_macro_step(index, cx);
                            })),
                    ),
            );
        }
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
            .child(labeled_field(
                "触发键（如 ctrl-alt-1，可留空）",
                &edit.trigger,
                pal,
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(div().text_sm().child(tr!(cx, "步骤（发送内容 + 步后延迟毫秒）")))
                            .child(
                                Button::new("macro-step-add")
                                    .label(tr!(cx, "+ 添加步骤"))
                                    .ghost()
                                    .compact()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_macro_step(window, cx);
                                    })),
                            ),
                    )
                    .child(steps)
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(tr!(
                                cx,
                                "转义：\\n 回车 · \\t Tab · \\e ESC · \\\\ 反斜杠；其余按字面发送。"
                            )),
                    ),
            )
            .child(div().text_xs().text_color(pal.danger).child(error))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("macro-save")
                            .label(tr!(cx, "保存"))
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.commit_macro_edit(cx);
                            })),
                    )
                    .child(
                        Button::new("macro-cancel")
                            .label(tr!(cx, "取消"))
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cancel_macro_edit(cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}
