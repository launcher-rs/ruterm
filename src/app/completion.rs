//! 终端内命令补全：边输入边在光标下方浮出候选，Tab / Enter 补全。
//!
//! 按键在 `App::intercept_keystrokes` 阶段处理（早于动作分发），所以有候选时
//! Tab / Enter / Esc / ↑↓ 不会被终端先吃掉；候选为空时全部原样放行。
//! 候选按词在命令行里的位置来给：命令位搜整库（敲「磁盘」能找到 `df -h`），
//! 参数位只接得上前文的写法（`git re` → `git remote …`、`git -` → 该命令的选项），
//! 接不上就不弹，把 Tab 交回 shell 自己的路径补全。
//! 补全只替换正在输入的那个词，替换前用终端网格核对，对不上就放弃。

use rgpui::prelude::*;
use rgpui::{
    AnyElement, Context, FontWeight, MouseButton, SharedString, Window, div, h_flex, px, v_flex,
};
use rgpui_term::TerminalContent;

use crate::completion::{
    CompletionItem, CompletionKind, complete, echo_lagging, last_token, merged_command,
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

/// 补全候选列表的缓存键：语言、会话平台或片段内容任一变化即失效。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct CompletionCacheKey {
    locale: &'static str,
    platform: Option<Platform>,
    /// 用户片段的内容指纹（哈希）：片段增删改都会变。
    snippets: u64,
    /// 命令历史的内容指纹：每回车一条命令都会变，候选里的历史条目要跟着换。
    history: u64,
}

/// 片段列表的内容指纹：只哈希字段字节，不分配。
fn snippet_fingerprint(snippets: &[CommandSnippet]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    snippets.iter().for_each(|snippet| {
        (
            snippet.name.as_str(),
            snippet.command.as_str(),
            snippet.note.as_str(),
        )
            .hash(&mut hasher)
    });
    hasher.finish()
}

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

    /// 候选：最近用过的历史命令在前，自己的片段其次，内置命令最后；名称与说明按当前界面语言翻译。
    /// 内置命令只取当前会话平台的：Linux 上不提示注册表命令，Windows 上不提示 apt。
    ///
    /// 列表按（语言、会话平台、片段内容指纹、历史内容指纹）缓存：补全在每次按键时重建，
    /// 200+ 内置命令逐条翻译加三条小写拷贝是热路径上纯浪费的分配。
    fn completion_items(&mut self, cx: &Context<Self>) -> Vec<CompletionItem> {
        let key = CompletionCacheKey {
            locale: crate::i18n::current_locale(cx),
            platform: self.active_session_platform(),
            snippets: snippet_fingerprint(&self.snippets),
            history: crate::completion::history_fingerprint(self.history.entries()),
        };
        if let Some((cached, items)) = &self.completion_cache
            && *cached == key
        {
            return items.clone();
        }
        let mine = self.snippets.iter().map(|snippet| {
            CompletionItem::new(
                snippet.name.clone(),
                snippet.command.clone(),
                snippet.note.clone(),
                CompletionKind::Snippet,
            )
        });
        let builtin = presets_for(self.active_session_platform()).map(|(_, preset)| {
            CompletionItem::new(
                crate::i18n::translate(cx, preset.name, &[]).to_string(),
                preset.command.to_string(),
                crate::i18n::translate(cx, preset.note, &[]).to_string(),
                CompletionKind::Command,
            )
        });
        let presets: Vec<CompletionItem> = mine.chain(builtin).collect();
        // 历史放池子最前：参数位的续写按池子顺序取前若干条，最近用过的写法要排在片段前面。
        let mut items = crate::completion::history_items(self.history.entries(), &presets);
        items.extend(presets);
        self.completion_cache = Some((key, items.clone()));
        items
    }

    /// 重新算候选；命令位词太短、参数位接不上前文、命令还在跑时都会收起浮层。
    fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        let Some(line) = self.typed_line.clone() else {
            self.close_completion(cx);
            return;
        };
        // 替换目标是行尾正在敲的那个词，它前面的文本决定这个词是命令还是参数。
        let token = last_token(&line).to_string();
        let before = line[..line.len().saturating_sub(token.len())].to_string();
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
        let hits = complete(&items, &token, &before);
        if hits.is_empty() {
            self.close_completion(cx);
            return;
        }
        let previous = self
            .completion
            .as_ref()
            .map(|state| state.selected)
            .unwrap_or(0);
        let selected = previous.min(hits.len().saturating_sub(1));
        self.completion = Some(CompletionState {
            tab_id,
            token,
            items: hits,
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
                // 词为空（刚敲完空格）时「退掉几个字符」是 0，网格对上与否失去锚点，
                // 只能再核对整行的记录；否则一旦网格与记录无关就会把整条命令插进去。
                let anchored = !token.is_empty() || before.ends_with(&tracked);
                if anchored
                    && before.len() >= token.len()
                    && before[before.len() - token.len()..] == token[..]
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
        self.status = crate::i18n::translate(cx, "已补全：{name}", &[("name", &item.name)]);
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
            // 选项候选补的就是它自己，第二列改放它来自哪条命令，免得两列重复；
            // 历史候选的名称就是整条命令，第二列再放一遍只是占地方。
            let detail = match item.kind {
                CompletionKind::Flag => item.note.as_str(),
                CompletionKind::History => "",
                _ => item.command.as_str(),
            };
            let badge = match item.kind {
                CompletionKind::Snippet => "",
                CompletionKind::Command => "内置",
                CompletionKind::Flag => "选项",
                CompletionKind::History => "历史",
            };
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
                    .child({
                        // 历史候选的名称是整条命令行，可能长过浮层宽度：让它参与
                        // 收缩并省略，别的候选保持按名称宽度占位。
                        let name = div()
                            .text_sm()
                            .font_weight(FontWeight(600.0))
                            .text_color(pal.text)
                            .child(SharedString::from(item.name.clone()));
                        if item.kind == CompletionKind::History {
                            name.flex_1().min_w_0().text_ellipsis()
                        } else {
                            name.flex_shrink_0()
                        }
                    })
                    .when(!detail.is_empty(), |this| {
                        this.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(pal.muted)
                                .text_ellipsis()
                                .child(SharedString::from(detail.to_string())),
                        )
                    })
                    .when(!badge.is_empty(), |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(pal.muted)
                                .child(crate::i18n::translate(cx, badge, &[])),
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
