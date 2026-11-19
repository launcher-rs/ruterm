//! 命令历史：入库、从终端 block 追踪回收、弹窗开关与复用（插入 / 执行 / 存片段）。

use std::collections::HashSet;

use rgpui::input_ui::{InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{Context, Window};

use crate::tr;
use crate::{CloseCommandHistory, CommandHistory as OpenCommandHistory, ToggleHistoryPanel};

use super::*;

impl RuTermApp {
    /// 历史落库：加密库 `settings.history`，库不可用时降级 `settings.json`。
    pub(super) fn persist_history(&self, cx: &mut Context<Self>) {
        let Some(vault) = self.vault.clone() else {
            // 明文降级：settings.json 不加密，剔除疑似内联凭据的条目——
            // 宁可历史断档，也不把密文落成明文（见 history.rs 的判定说明）。
            self.store.save_history(&self.history.to_json_scrubbed());
            return;
        };
        let json = self.history.to_json();
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("history", &json).await {
                log::warn!("写入库内命令历史失败: {error}");
            }
        })
        .detach();
    }

    /// 记一条命令：去重、裁剪与落库都在这里收口。
    pub(super) fn push_history(&mut self, command: &str, cx: &mut Context<Self>) {
        if self.history.push(command) {
            self.history_clear_confirm = false;
            self.persist_history(cx);
            cx.notify();
        }
    }

    /// 从各标签的 block 追踪里收新命令。
    ///
    /// 只在终端回显过命令行（回车时网格上有内容）才产生 block，因此密码输入、
    /// vim 等程序里的回车都不会误收。程序化发送不走 block 追踪，由调用方
    /// 直接 `push_history`。
    pub(super) fn sync_command_history(&mut self, cx: &mut Context<Self>) {
        let mut fresh: Vec<String> = Vec::new();
        let mut alive: HashSet<usize> = HashSet::new();
        for tab in self.tabs.iter() {
            alive.insert(tab.id);
            let Some(view) = tab.terminal_view.as_ref() else {
                continue;
            };
            let terminal = view.read(cx).terminal().clone();
            let blocks = terminal.read(cx).blocks();
            let seen = self.history_seen.get(&tab.id).copied().unwrap_or(0);
            // 重连会换一个全新的终端，block id 从 0 重来；发现计数倒退就重新收。
            let latest = blocks.last().map(|block| block.id + 1).unwrap_or(0);
            let seen = if latest < seen { 0 } else { seen };
            let mut next = seen;
            for block in blocks.iter().filter(|block| block.id >= seen) {
                next = next.max(block.id + 1);
                fresh.push(block.command.clone());
            }
            if next != seen || !self.history_seen.contains_key(&tab.id) {
                self.history_seen.insert(tab.id, next);
            }
        }
        self.history_seen.retain(|id, _| alive.contains(id));
        let mut changed = false;
        for command in fresh {
            changed |= self.history.push(&command);
        }
        if changed {
            self.history_clear_confirm = false;
            self.persist_history(cx);
            cx.notify();
        }
    }

    /// Ctrl+Shift+H / 导航栏「历史」图标：开关右侧历史面板。
    pub(super) fn toggle_history_panel_action(
        &mut self,
        _: &ToggleHistoryPanel,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_history_panel(cx);
    }

    pub(super) fn toggle_history_panel(&mut self, cx: &mut Context<Self>) {
        self.history_panel_open = !self.history_panel_open;
        self.history_clear_confirm = false;
        if self.history_panel_open {
            // 刚执行过的命令不用等下一轮轮询才出现在列表里。
            self.sync_command_history(cx);
        }
        cx.notify();
    }

    /// 「终端」菜单：打开命令历史管理弹窗。
    pub(super) fn open_history_manager(
        &mut self,
        _: &OpenCommandHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_history_manager_impl(window, cx);
    }

    /// 打开历史弹窗；已打开时只把焦点放回搜索框。
    pub(super) fn open_history_manager_impl(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history_clear_confirm = false;
        if let Some(state) = self.history_manager.as_ref() {
            state.search.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
            return;
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "搜索命令历史…")));
        cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        search.update(cx, |input, cx| input.focus(window, cx));
        self.history_manager = Some(HistoryManager {
            search,
            confirm_clear: false,
        });
        // 打开时先扫一遍：刚执行过的命令不用等下一轮轮询才出现。
        self.sync_command_history(cx);
        cx.notify();
    }

    pub(super) fn close_history_manager_action(
        &mut self,
        _: &CloseCommandHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_history_manager(window, cx);
    }

    pub(super) fn close_history_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history_manager.take().is_none() {
            return;
        }
        if let Some(view) = self.active_terminal_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// 删除一条历史。凭命令串定位而非下标：历史列表会被 250ms 轮询不断
    /// `insert(0, …)` 移位，渲染时捕获的下标在点击时可能指向另一条命令。
    /// 条目在 `push` 里去重，命令串即身份。
    pub(super) fn delete_history_entry(&mut self, command: &str, cx: &mut Context<Self>) {
        let Some(index) = self
            .history
            .entries()
            .iter()
            .position(|entry| entry == command)
        else {
            return;
        };
        if self.history.remove(index) {
            self.history_clear_confirm = false;
            self.persist_history(cx);
            cx.notify();
        }
    }

    /// 清空历史（两步确认：第一次只把按钮换成「确认清空」）。
    pub(super) fn request_clear_history(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.history_manager.as_mut() else {
            return;
        };
        if !state.confirm_clear {
            state.confirm_clear = true;
            cx.notify();
            return;
        }
        state.confirm_clear = false;
        if self.history.clear() {
            self.persist_history(cx);
            self.status = tr!(cx, "已清空命令历史");
        }
        cx.notify();
    }

    /// 历史面板的清空：与弹窗一致的两步确认，确认态只活在面板上。
    pub(super) fn request_clear_history_panel(&mut self, cx: &mut Context<Self>) {
        if !self.history_clear_confirm {
            self.history_clear_confirm = true;
            cx.notify();
            return;
        }
        self.history_clear_confirm = false;
        if self.history.clear() {
            self.persist_history(cx);
            self.status = tr!(cx, "已清空命令历史");
        }
        cx.notify();
    }

    /// 插入 / 执行历史命令到当前活动会话，返回是否真的送出。
    /// 凭命令串取当前条目：条目仍存在时用库内最新内容，已被删除则不动。
    pub(super) fn send_history_command(
        &mut self,
        command: &str,
        run: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(current) = self
            .history
            .entries()
            .iter()
            .find(|entry| entry.as_str() == command)
            .cloned()
        else {
            return false;
        };
        if !self.send_command_to_terminal(&current, run, cx) {
            return false;
        }
        let key = if run {
            "已执行历史命令"
        } else {
            "已插入历史命令"
        };
        self.status = crate::i18n::translate(cx, key, &[]);
        cx.notify();
        true
    }

    /// 面板里点一行 / 点 ▶：送出后把焦点交回终端，接着敲键盘就落在会话里。
    pub(super) fn use_history_entry(
        &mut self,
        command: &str,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.send_history_command(command, run, cx) {
            return;
        }
        if let Some(view) = self.active_terminal_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    /// 把历史命令复制进片段编辑器：名称默认取命令开头，备注标注来源。
    pub(super) fn save_history_as_snippet(
        &mut self,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = self
            .history
            .entries()
            .iter()
            .find(|entry| entry.as_str() == command)
            .cloned()
        else {
            return;
        };
        // 历史弹窗先收起，避免盖住右侧片段面板的内联编辑器。
        self.history_manager = None;
        let name: String = command.chars().take(32).collect();
        self.begin_snippet_edit_values(None, &name, &command, "来自命令历史", window, cx);
        cx.notify();
    }
}
