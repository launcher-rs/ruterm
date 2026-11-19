//! //! 终端输入侧操作：查找条（Ctrl+F）、同步输入（MirrorInput 中间件）与命令广播。

use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    Anchor, AnyElement, Button, ButtonVariants, Checkbox, Context, Disableable, DismissEvent,
    Entity, Focusable, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, PopupMenu, SharedString, Styled, Window, anchored, deferred, div,
    h_flex, px, v_flex,
};
use rgpui_term::{InputOrigin, TerminalMiddleware, TerminalView};

use crate::tr;
use crate::{
    BroadcastCommand, CancelBroadcast, CloseFind, FindInTerminal, TerminalCopy, TerminalCopyText,
    TerminalPaste, ToggleSyncInput,
};

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

/// 粘贴行尾归一化：Windows 剪贴板的多行文本是 CRLF，而 bracketed-paste 模式下
/// 终端框架会把原文原样包进 `ESC[200~ … ESC[201~` 发给远端。远端 readline 会把
/// 其中的 `\r` 当普通字符留在行缓冲里（回显 `^M`），行尾 `\` 也续不上行；这里在
/// 字节进 PTY 前把括号内的 CRLF / 孤立 CR 折成 LF。
///
/// 只处理 bracketed 包裹的粘贴：未开 bracketed-paste 时框架已把换行转成 `\r`
/// （回车），不能再动，否则 vim 之类的原始模式程序收不到回车。
pub(super) struct NormalizePastedLineEndings;

impl TerminalMiddleware for NormalizePastedLineEndings {
    fn on_input(
        &self,
        input: std::borrow::Cow<'static, [u8]>,
        origin: InputOrigin,
    ) -> Option<std::borrow::Cow<'static, [u8]>> {
        if origin != InputOrigin::Paste {
            return Some(input);
        }
        let Ok(text) = std::str::from_utf8(&input) else {
            return Some(input);
        };
        let Some(inner) = text
            .strip_prefix("\x1b[200~")
            .and_then(|rest| rest.strip_suffix("\x1b[201~"))
        else {
            return Some(input);
        };
        if !inner.as_bytes().contains(&b'\r') {
            return Some(input);
        }
        let inner = inner.replace("\r\n", "\n").replace('\r', "\n");
        Some(format!("\x1b[200~{inner}\x1b[201~").into_bytes().into())
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

/// 终端右键菜单：菜单实体 + 弹出位置（窗口坐标）。
///
/// 菜单放在 root 的 `deferred` 图层里渲染，命中区在终端之上；点外部或按 Esc
/// 由 `PopupMenu` 自己关闭，并通过 `DismissEvent` 把这里的状态清掉。
pub(super) struct TerminalMenu {
    pub(super) position: Point<Pixels>,
    pub(super) menu: Entity<PopupMenu>,
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

    pub(super) fn close_find(
        &mut self,
        _: &CloseFind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            .and_then(|tab| tab.terminal_view.clone())
    }

    /// 持有键盘焦点的终端视图（分屏时可能是非活动标签）；找不到时回落到活动标签。
    fn focused_terminal(&self, window: &Window, cx: &App) -> Option<Entity<TerminalView>> {
        let focused = window.focused(cx)?;
        self.tabs
            .iter()
            .filter_map(|tab| tab.terminal_view.as_ref())
            .find(|view| view.read(cx).focus_handle(cx) == focused)
            .cloned()
    }

    /// 按标签 id 找终端视图，并给出会话是否还能收输入。
    fn terminal_view_by_id(&self, tab_id: usize) -> Option<(Entity<TerminalView>, bool)> {
        let tab = self.tabs.iter().find(|tab| tab.id == tab_id)?;
        let view = tab.terminal_view.clone()?;
        Some((view, !tab.ended && !tab.failed))
    }

    /// 复制终端当前选区，返回是否真的有选区。
    ///
    /// 走终端自身的 `copy`：它在下次 `sync` 时写剪贴板并清掉选区
    /// （与 Windows Terminal 一致，复制后高亮消失即"已复制"的反馈）。
    fn copy_terminal_selection(&self, view: &Entity<TerminalView>, cx: &mut Context<Self>) -> bool {
        let terminal = view.read(cx).terminal().clone();
        if terminal.read(cx).last_content().selection.is_none() {
            return false;
        }
        terminal.update(cx, |terminal, _| terminal.copy(Some(false)));
        cx.notify();
        true
    }

    /// 把系统剪贴板里的文本粘进终端（bracketed-paste / CRLF 归一化由终端负责）。
    ///
    /// 多行文本先弹确认：未开 bracketed-paste 的目标会把换行当逐行回车立即
    /// 执行（iTerm2 式危险粘贴保护）。`tab_id` 解析不出时跳过确认直接粘。
    fn paste_terminal(
        &mut self,
        tab_id: Option<usize>,
        view: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .filter(|text| !text.is_empty())
        else {
            return;
        };
        if let Some(tab_id) = tab_id
            && !self.paste_confirm_disabled
            && text.lines().count() >= 2
        {
            self.confirm_paste = Some((tab_id, text));
            self.paste_confirm_checked = false;
            cx.notify();
            return;
        }
        let terminal = view.read(cx).terminal().clone();
        terminal.update(cx, |terminal, _| terminal.paste(&text));
    }

    /// 确认粘贴：执行确认时存下的文本；勾选了「不再提示」则本次运行关闭询问。
    pub(super) fn confirm_paste_now(&mut self, cx: &mut Context<Self>) {
        let Some((tab_id, text)) = self.confirm_paste.take() else {
            return;
        };
        if self.paste_confirm_checked {
            self.paste_confirm_disabled = true;
        }
        let Some((view, live)) = self.terminal_view_by_id(tab_id) else {
            return;
        };
        if live {
            let terminal = view.read(cx).terminal().clone();
            terminal.update(cx, |terminal, _| terminal.paste(&text));
        }
        cx.notify();
    }

    pub(super) fn cancel_paste_confirm(&mut self, cx: &mut Context<Self>) {
        if self.confirm_paste.take().is_some() {
            self.paste_confirm_checked = false;
            cx.notify();
        }
    }

    fn copy_terminal_by_id(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let Some((view, _)) = self.terminal_view_by_id(tab_id) else {
            return;
        };
        self.copy_terminal_selection(&view, cx);
    }

    fn paste_terminal_by_id(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let Some((view, live)) = self.terminal_view_by_id(tab_id) else {
            return;
        };
        if live {
            self.paste_terminal(Some(tab_id), &view, cx);
        }
    }

    fn select_all_terminal(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let Some((view, _)) = self.terminal_view_by_id(tab_id) else {
            return;
        };
        let terminal = view.read(cx).terminal().clone();
        terminal.update(cx, |terminal, _| terminal.select_all());
    }

    fn clear_terminal_by_id(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let Some((view, _)) = self.terminal_view_by_id(tab_id) else {
            return;
        };
        let terminal = view.read(cx).terminal().clone();
        terminal.update(cx, |terminal, _| terminal.clear());
    }

    /// Ctrl+C：有选区复制，没有选区就把 ^C 送给 shell（经典终端语义）。
    pub(super) fn terminal_copy_action(
        &mut self,
        _: &TerminalCopy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self
            .focused_terminal(window, cx)
            .or_else(|| self.active_terminal_view());
        if let Some(view) = view
            && !self.copy_terminal_selection(&view, cx)
        {
            let terminal = view.read(cx).terminal().clone();
            // 用 Keystroke 来源而不是 Programmatic：开了「同步输入」时 Ctrl+C
            // 也要一并中断其它会话。
            terminal.update(cx, |terminal, _| {
                terminal.input_with_origin(vec![0x03], InputOrigin::Keystroke);
            });
        }
        // 必须掐断动作分发：否则绑定处理完还会走到 TerminalView 的 on_key_down，
        // 把 ^C 再送一遍（有选区时就会"复制 + 中断"同时发生）。
        cx.stop_propagation();
    }

    /// Ctrl+Shift+C：只复制，没有选区就什么都不做（绝不误发中断）。
    pub(super) fn terminal_copy_text_action(
        &mut self,
        _: &TerminalCopyText,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .focused_terminal(window, cx)
            .or_else(|| self.active_terminal_view())
        {
            self.copy_terminal_selection(&view, cx);
        }
        cx.stop_propagation();
    }

    /// Ctrl+V / Ctrl+Shift+V / Shift+Insert：把剪贴板文本粘进终端。
    pub(super) fn terminal_paste_action(
        &mut self,
        _: &TerminalPaste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .focused_terminal(window, cx)
            .or_else(|| self.active_terminal_view())
        {
            let tab_id = self
                .tabs
                .iter()
                .find(|tab| tab.terminal_view.as_ref() == Some(&view))
                .map(|tab| tab.id);
            let live = self
                .tabs
                .iter()
                .any(|tab| tab.terminal_view.as_ref() == Some(&view) && !tab.ended && !tab.failed);
            if live {
                self.paste_terminal(tab_id, &view, cx);
            }
        }
        // 没有可粘贴的会话时也要吞掉按键，避免 ^V 落到 shell。
        cx.stop_propagation();
    }

    /// 终端区域右键：先于框架的「右键=粘贴」拦下事件，改为弹剪贴板菜单。
    ///
    /// `TerminalView` 在冒泡阶段直接粘贴且没有开关，所以只能在 pane 的捕获阶段
    /// `stop_propagation()`：冒泡阶段整个跳过，也就不会误粘。鼠标上报模式
    ///（vim / mc 等）下右键属于远端程序，原样放行。
    pub(super) fn maybe_open_terminal_menu(
        &mut self,
        tab_id: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Right {
            return;
        }
        let Some((view, _)) = self.terminal_view_by_id(tab_id) else {
            return;
        };
        let terminal = view.read(cx).terminal().clone();
        let intercept = {
            let terminal = terminal.read(cx);
            !terminal.mouse_mode(event.modifiers.shift)
                && terminal
                    .last_content()
                    .terminal_bounds
                    .bounds
                    .contains(&event.position)
        };
        if !intercept {
            return;
        }
        cx.stop_propagation();
        // 分屏时右键另一侧也要切过去：菜单关闭后焦点回到终端，活动标签不能还
        // 停在旧会话上（与左键点击面板一致）。
        if self.active_tab != Some(tab_id) {
            self.activate_tab(tab_id, window, cx);
        }
        self.open_terminal_menu(tab_id, event.position, window, cx);
    }

    fn open_terminal_menu(
        &mut self,
        tab_id: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((view, live)) = self.terminal_view_by_id(tab_id) else {
            return;
        };
        let terminal = view.read(cx).terminal().clone();
        let focus = view.read(cx).focus_handle(cx);
        let has_selection = terminal.read(cx).last_content().selection.is_some();
        let has_clipboard_text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .is_some_and(|text| !text.is_empty());
        let entity = cx.entity();
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            // 关闭菜单时焦点回到终端；否则键盘接着敲就落不到 PTY 上。
            let menu = menu.min_w(px(180.0)).action_context(focus.clone());
            menu.item(
                Self::menu_item(
                    entity.clone(),
                    tr!(cx, "复制"),
                    None,
                    false,
                    false,
                    move |this, _, cx| this.copy_terminal_by_id(tab_id, cx),
                )
                .disabled(!has_selection),
            )
            .item(
                Self::menu_item(
                    entity.clone(),
                    tr!(cx, "粘贴"),
                    None,
                    false,
                    false,
                    move |this, _, cx| this.paste_terminal_by_id(tab_id, cx),
                )
                .disabled(!live || !has_clipboard_text),
            )
            .separator()
            .item(Self::menu_item(
                entity.clone(),
                tr!(cx, "全选"),
                None,
                false,
                false,
                move |this, _, cx| this.select_all_terminal(tab_id, cx),
            ))
            .item(Self::menu_item(
                entity.clone(),
                tr!(cx, "清屏"),
                None,
                false,
                false,
                move |this, _, cx| this.clear_terminal_by_id(tab_id, cx),
            ))
        });
        menu.focus_handle(cx).focus(window, cx);
        // 菜单自己处理 Esc / 点外部；关闭时把应用侧状态一并清掉。
        cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.terminal_menu = None;
            cx.notify();
        })
        .detach();
        self.terminal_menu = Some(TerminalMenu { position, menu });
        cx.notify();
    }

    /// 右键菜单的独立图层（root 最后绘制，命中区盖在终端与状态栏之上）。
    pub(super) fn render_terminal_menu(&mut self) -> AnyElement {
        // 打开设置等居中模态后收起，避免菜单隔着模态显示、再次关闭时又冒出来。
        if self.overlay_open() {
            self.terminal_menu = None;
            return div().into_any_element();
        }
        let Some(state) = self.terminal_menu.as_ref() else {
            return div().into_any_element();
        };
        let menu = state.menu.clone();
        deferred(
            anchored()
                .snap_to_window_with_margin(px(8.0))
                .anchor(Anchor::TopLeft)
                .position(state.position)
                .child(menu),
        )
        .with_priority(1)
        .into_any_element()
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

    /// 可接收输入的会话数（不含已结束 / 连接失败的标签；远程桌面没有终端输入通道）。
    pub(super) fn live_tab_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|tab| !tab.ended && !tab.failed && tab.terminal_view.is_some())
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
            // 开启即警示：镜像在输入路径上，回显与否无关，sudo/口令提示符下
            // 敲的密码同样会被转发并回显到其它会话的屏幕上（可被历史收录）。
            // 前缀须保持「同步输入」，tabs.rs 的 prune 靠它识别并覆盖本条消息。
            SharedString::from(format!(
                "同步输入：开（{live} 个会话）——所有按键都会原样转发到其它会话，密码输入也不例外，敲敏感信息前请先关闭"
            ))
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
            let Some(view) = tab.terminal_view.as_ref() else {
                continue;
            };
            let terminal = view.read(cx).terminal().clone();
            // 走 `input()`＝Programmatic 来源，中间件不再镜像，避免回流。
            terminal.update(cx, |terminal, _| terminal.input(payload.clone()));
        }
    }

    /// 把一条命令写进活动终端：`run` 时追加回车。返回是否真的送出。
    ///
    /// 片段 / 内置命令 / 历史复用同一条路径；「执行」也会立刻记进命令历史，
    /// 因为 Programmatic 输入不经过 block 追踪。
    pub(super) fn send_command_to_terminal(
        &mut self,
        command: &str,
        run: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let active = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id));
        let Some(tab) = active else {
            self.status = tr!(cx, "没有可发送命令的终端会话");
            cx.notify();
            return false;
        };
        if tab.ended {
            self.status = tr!(cx, "会话已断开，无法发送命令");
            cx.notify();
            return false;
        }
        let Some(view) = tab.terminal_view.clone() else {
            self.status = tr!(cx, "没有可发送命令的终端会话");
            cx.notify();
            return false;
        };
        let terminal = view.read(cx).terminal().clone();
        let mut bytes = command.trim_end_matches(['\r', '\n']).as_bytes().to_vec();
        if run {
            bytes.push(b'\r');
        }
        terminal.update(cx, |terminal, _| terminal.input(bytes));
        if run {
            self.push_history(command, cx);
        }
        true
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
        let mut bytes = command.clone().into_bytes();
        bytes.push(b'\r');
        let mut targets = 0;
        for tab in self.tabs.iter() {
            if tab.ended || tab.failed {
                continue;
            }
            let Some(view) = tab.terminal_view.as_ref() else {
                continue;
            };
            let terminal = view.read(cx).terminal().clone();
            terminal.update(cx, |terminal, _| terminal.input(bytes.clone()));
            targets += 1;
        }
        self.broadcast = None;
        // 广播是程序化输入，不产生 block，命令历史在这里补记一次。
        self.push_history(&command, cx);
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
            .map(|tab| self.tab_display_title(tab.id).to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn transform(input: &[u8], origin: InputOrigin) -> Vec<u8> {
        NormalizePastedLineEndings
            .on_input(input.to_vec().into(), origin)
            .expect("输入不应被丢弃")
            .into_owned()
    }

    #[test]
    fn bracketed_paste_crlf_becomes_lf() {
        assert_eq!(
            transform(
                b"\x1b[200~echo one \\\r\n  two\r\n\x1b[201~",
                InputOrigin::Paste
            ),
            b"\x1b[200~echo one \\\n  two\n\x1b[201~"
        );
    }

    #[test]
    fn bracketed_paste_lone_cr_becomes_lf() {
        assert_eq!(
            transform(b"\x1b[200~one\rtwo\x1b[201~", InputOrigin::Paste),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
    }

    #[test]
    fn bracketed_paste_without_cr_is_untouched() {
        let input = b"\x1b[200~one\ntwo\x1b[201~";
        assert_eq!(transform(input, InputOrigin::Paste), input);
    }

    #[test]
    fn unbracketed_paste_is_untouched() {
        let input = b"one\rtwo\r";
        assert_eq!(transform(input, InputOrigin::Paste), input);
    }

    #[test]
    fn other_origins_are_untouched() {
        let input = b"\x1b[200~one\r\ntwo\x1b[201~";
        assert_eq!(transform(input, InputOrigin::Text), input);
    }
}
