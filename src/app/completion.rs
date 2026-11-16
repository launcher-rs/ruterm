//! 终端内命令补全：边输入边在光标下方浮出命令片段候选，Tab / Enter 补全。
//!
//! 按键在 `App::intercept_keystrokes` 阶段处理（早于动作分发），所以有候选时
//! Tab / Enter / Esc / ↑↓ 不会被终端先吃掉；候选为空时全部原样放行。
//! 补全只替换正在输入的那个词，替换前用终端网格核对，对不上就放弃。

use rgpui::prelude::*;
use rgpui::{
    AnyElement, Context, FontWeight, MouseButton, SharedString, Window, div, h_flex, px, v_flex,
};
use rgpui_term::TerminalContent;

use crate::completion::{
    CompletionItem, MIN_TOKEN_CHARS, echo_lagging, last_token, matches_after, merged_command,
};
use crate::snippet_presets::presets_for;
use crate::tr;

use super::*;

/// 浮层状态：只对正在输入的那个标签生效。
pub(super) struct CompletionState {
    pub(super) tab_id: usize,
    /// 正在输入的词，接受补全时按它算要退掉几个字符。
    pub(super) token: String,
    /// 当前候选（已经按匹配度排好）。
    pub(super) items: Vec<CompletionItem>,
    pub(super) selected: usize,
}

/// 浮层宽度与单行高度（像素）：命令通常比名称长，给宽一点。
const POPUP_WIDTH: f32 = 440.0;
const ROW_HEIGHT: f32 = 26.0;
const FOOTER_HEIGHT: f32 = 26.0;

impl RuTermApp {
    /// 安装按键拦截器；实体销毁后自动失效。
    pub(super) fn install_completion_interceptor(cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        cx.intercept_keystrokes(move |event, window, cx| {
            if let Some(entity) = this.upgrade() {
                entity.update(cx, |this, cx| {
                    this.intercept_completion_key(event, window, cx)
                });
            }
        })
        .detach();
    }

    fn intercept_completion_key(
        &mut self,
        event: &rgpui::KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 设置里关掉自动补全后，按键完全交还给终端。
        if !self.terminal.inline_completion {
            self.close_completion(cx);
            return;
        }
        // 焦点不在终端（弹窗输入框、侧边栏）时补全一律让路。
        let in_terminal = event
            .context_stack
            .iter()
            .any(|context| context.contains("Terminal"));
        if !in_terminal || self.overlay_open() {
            self.close_completion(cx);
            return;
        }
        let Some(view) = self.active_terminal_view() else {
            self.close_completion(cx);
            return;
        };
        if !view.read(cx).focus_handle(cx).is_focused(window) {
            self.close_completion(cx);
            return;
        }
        let terminal = view.read(cx).terminal().clone();

        let stroke = &event.keystroke;
        let key = stroke.key.as_str();
        if self.completion.is_some() {
            match key {
                "up" | "down" => {
                    self.move_completion(key == "down", cx);
                    cx.stop_propagation();
                    return;
                }
                "tab" | "enter" => {
                    // 两键都只补全不执行：补全后命令停在行尾，再回车才跑。
                    self.accept_completion(cx);
                    cx.stop_propagation();
                    return;
                }
                "escape" => {
                    self.close_completion(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        // 输入法组合期间按键归输入法，上屏文本不走 keystroke，不跟踪。
        if terminal.read(cx).ime_composition().is_some() {
            return;
        }

        let secondary = stroke.modifiers.control
            || stroke.modifiers.alt
            || stroke.modifiers.platform
            || stroke.modifiers.function;
        match key {
            "enter" => self.typed_line = Some(String::new()),
            // Ctrl+C / Ctrl+U 都意味着新的一行，从行首重新累计。
            "c" | "u" if stroke.modifiers.control => {
                self.typed_line = Some(String::new());
            }
            "backspace" => {
                if let Some(line) = self.typed_line.as_mut() {
                    line.pop();
                }
            }
            // 行内移动光标后，记录的行尾未必还贴着光标，等回车开新行再恢复。
            "left" | "right" | "home" | "end" | "delete" | "pageup" | "pagedown" => {
                self.typed_line = None;
            }
            _ => {
                if secondary {
                    self.typed_line = None;
                } else if let Some(text) = stroke.key_char.as_deref() {
                    if let Some(line) = self.typed_line.as_mut() {
                        line.push_str(text);
                    }
                } else {
                    self.typed_line = None;
                }
            }
        }
        self.refresh_completion(cx);
    }

    /// 候选：自己的片段在前，内置命令在后；名称与说明按当前界面语言翻译。
    /// 内置命令只取当前会话平台的：Linux 上不提示注册表命令，Windows 上不提示 apt。
    fn completion_items(&self, cx: &Context<Self>) -> Vec<CompletionItem> {
        let mine = self
            .snippets
            .iter()
            .enumerate()
            .map(|(index, snippet)| CompletionItem {
                name: snippet.name.clone(),
                command: snippet.command.clone(),
                note: snippet.note.clone(),
                builtin: false,
                index,
            });
        let builtin =
            presets_for(self.active_session_platform()).map(|(index, preset)| CompletionItem {
                name: crate::i18n::translate(cx, preset.name, &[]).to_string(),
                command: preset.command.to_string(),
                note: crate::i18n::translate(cx, preset.note, &[]).to_string(),
                builtin: true,
                index,
            });
        mine.chain(builtin).collect()
    }

    /// 重新算候选；词太短、没命中或命令还在跑时收起浮层。
    fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        let Some(line) = self.typed_line.clone() else {
            self.close_completion(cx);
            return;
        };
        // 替换目标是行尾正在敲的那个词；词还太短时退回上一个词做查询，
        // 这样 `docker p` / `docker ` 仍能列出 docker 相关命令。
        let token = last_token(&line).to_string();
        let query = if token.chars().count() >= MIN_TOKEN_CHARS {
            token.clone()
        } else {
            let before = line[..line.len().saturating_sub(token.len())].trim_end();
            last_token(before).to_string()
        };
        if query.chars().count() < MIN_TOKEN_CHARS {
            self.close_completion(cx);
            return;
        }
        let Some(tab_id) = self.active_tab else {
            self.close_completion(cx);
            return;
        };
        let Some(view) = self.active_terminal_view() else {
            self.close_completion(cx);
            return;
        };
        let terminal = view.read(cx).terminal().clone();
        // 命令还在跑（vim / ssh / 本地程序）时输入的不是 shell 命令行，不弹。
        if terminal.read(cx).block_running() {
            self.close_completion(cx);
            return;
        }
        let items = self.completion_items(cx);
        let prefix = &line[..line.len().saturating_sub(token.len())];
        let hits = matches_after(&items, &query, prefix);
        if hits.is_empty() {
            self.close_completion(cx);
            return;
        }
        let items: Vec<CompletionItem> = hits.into_iter().map(|slot| items[slot].clone()).collect();
        let previous = self
            .completion
            .as_ref()
            .map(|state| state.selected)
            .unwrap_or(0);
        let selected = previous.min(items.len().saturating_sub(1));
        self.completion = Some(CompletionState {
            tab_id,
            token,
            items,
            selected,
        });
        cx.notify();
    }

    fn move_completion(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(state) = self.completion.as_mut() else {
            return;
        };
        let len = state.items.len();
        if len == 0 {
            return;
        }
        state.selected = if down {
            (state.selected + 1) % len
        } else {
            (state.selected + len - 1) % len
        };
        cx.notify();
    }

    /// 收起浮层（焦点离开终端、切标签、词变短时都会走到这里）。
    pub(super) fn close_completion(&mut self, cx: &mut Context<Self>) {
        if self.completion.take().is_some() {
            cx.notify();
        }
    }

    /// 接受选中项：退掉正在输入的词，换成完整命令，不追加回车。
    fn accept_completion(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.completion.take() else {
            return;
        };
        let Some(item) = state.items.get(state.selected).cloned() else {
            cx.notify();
            return;
        };
        let Some(view) = self.active_terminal_view() else {
            cx.notify();
            return;
        };
        let terminal = view.read(cx).terminal().clone();
        // 只信终端网格：光标前的字符确实等于记录的词时才替换，避免光标
        // 被别的程序挪过位置还盲删字符。滚出底部 / 对不上就放弃；但如果只是
        // 远程回显还没追上按键，网格行尾会是记录整行的前缀，按队列顺序替换仍安全。
        let token: Vec<char> = state.token.chars().collect();
        let tracked: Vec<char> = self
            .typed_line
            .clone()
            .unwrap_or_default()
            .chars()
            .collect();
        let prefix = {
            let term = terminal.read(cx);
            let content = term.last_content();
            if content.display_offset != 0 {
                None
            } else {
                let line = content.cursor.point.line.0;
                let column = content.cursor.point.column.0;
                let before = line_chars_before(content, line, column);
                if before.len() >= token.len() && before[before.len() - token.len()..] == token[..]
                {
                    Some(
                        before[..before.len() - token.len()]
                            .iter()
                            .collect::<String>(),
                    )
                } else if tracked.ends_with(&token[..])
                    && echo_lagging(&before, &tracked, token.len())
                {
                    Some(
                        tracked[..tracked.len() - token.len()]
                            .iter()
                            .collect::<String>(),
                    )
                } else {
                    None
                }
            }
        };
        let Some(prefix) = prefix else {
            cx.notify();
            return;
        };
        let merged = merged_command(&prefix, &item.command);
        let mut bytes = vec![0x7f; token.len()];
        bytes.extend_from_slice(merged.as_bytes());
        terminal.update(cx, |terminal, _| terminal.input(bytes));
        // 记录行同步成本地拼好的整行，后续继续输入时还能接着匹配。
        let mut tracked = tracked;
        tracked.truncate(tracked.len().saturating_sub(token.len()));
        self.typed_line = Some(format!(
            "{}{}",
            tracked.into_iter().collect::<String>(),
            merged
        ));
        self.status = crate::i18n::translate(cx, "已补全片段：{name}", &[("name", &item.name)]);
        cx.notify();
    }

    fn pick_completion(&mut self, slot: usize, cx: &mut Context<Self>) {
        if let Some(state) = self.completion.as_mut() {
            if slot >= state.items.len() {
                return;
            }
            state.selected = slot;
        }
        self.accept_completion(cx);
    }

    /// 浮层：贴在光标下一行；下方放不下就翻到光标上方。
    pub(super) fn render_completion_popup(
        &self,
        tab_id: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = self
            .completion
            .as_ref()
            .filter(|state| state.tab_id == tab_id)
        else {
            return div().into_any_element();
        };
        let Some(view) = self.active_terminal_view() else {
            return div().into_any_element();
        };
        let pal = self.palette(cx);
        let terminal = view.read(cx).terminal().clone();
        let (left, top) = {
            let term = terminal.read(cx);
            let content = term.last_content();
            let bounds = content.terminal_bounds;
            let cell_w = bounds.cell_width.as_f32();
            let line_h = bounds.line_height.as_f32();
            let pane_w = bounds.bounds.size.width.as_f32();
            let pane_h = bounds.bounds.size.height.as_f32();
            let cursor = content.cursor.point;
            let content_h = state.items.len() as f32 * ROW_HEIGHT + FOOTER_HEIGHT + 12.0;
            let x = cursor.column.0 as f32 * cell_w;
            let left = x.min((pane_w - POPUP_WIDTH - 8.0).max(0.0));
            let below = (cursor.line.0 + 1) as f32 * line_h + 2.0;
            let top = if below + content_h <= pane_h {
                below
            } else {
                (cursor.line.0 as f32 * line_h - content_h - 2.0).max(0.0)
            };
            (px(left), px(top))
        };

        let mut rows = v_flex().gap(px(1.0)).py(px(2.0));
        for (slot, item) in state.items.iter().enumerate() {
            let selected = slot == state.selected;
            rows = rows.child(
                h_flex()
                    .id(("completion-row", slot))
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .when(selected, |this| this.bg(pal.card_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.pick_completion(slot, cx)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .font_weight(FontWeight(600.0))
                            .text_color(pal.text)
                            .child(SharedString::from(item.name.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(pal.muted)
                            .text_ellipsis()
                            .child(SharedString::from(item.command.clone())),
                    )
                    .when(item.builtin, |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(pal.muted)
                                .child(tr!(cx, "内置")),
                        )
                    }),
            );
        }

        v_flex()
            .id("completion-popup")
            .absolute()
            .left(left)
            .top(top)
            .w(px(POPUP_WIDTH))
            .p_1()
            .rounded(px(6.0))
            .bg(pal.card_bg)
            .border_1()
            .border_color(pal.border)
            .shadow_md()
            .child(rows)
            .child(
                div()
                    .px_2()
                    .py(px(2.0))
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(cx, "↑↓ 选择 · Tab 补全 · Esc 关闭")),
            )
            .into_any_element()
    }
}

/// 光标所在行、光标之前的字符（按列排序）。宽字符占两列，这里只取字符本身。
fn line_chars_before(content: &TerminalContent, line: i32, column: usize) -> Vec<char> {
    let mut cells: Vec<(usize, char)> = content
        .cells
        .iter()
        .filter(|indexed| indexed.point.line.0 == line && indexed.point.column.0 < column)
        .map(|indexed| (indexed.point.column.0, indexed.cell.c))
        .collect();
    cells.sort_by_key(|(column, _)| *column);
    cells.into_iter().map(|(_, ch)| ch).collect()
}
