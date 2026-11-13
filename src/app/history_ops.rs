//! 命令历史：入库、从终端 block 追踪回收、弹窗开关与复用（插入 / 执行 / 存片段）。

use std::collections::HashSet;

use rgpui::input_ui::{InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{Context, Window};

use crate::tr;
use crate::{CloseCommandHistory, CommandHistory as OpenCommandHistory};

use super::*;

impl RuTermApp {
    /// 历史落库：加密库 `settings.history`，库不可用时降级 `settings.json`。
    pub(super) fn persist_history(&self, cx: &mut Context<Self>) {
        let json = self.history.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_history(&json);
            return;
        };
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
            self.persist_history(cx);
            cx.notify();
        }
    }

    /// Ctrl+Shift+H /「终端」菜单：打开命令历史。
    pub(super) fn open_history_manager(
        &mut self,
        _: &OpenCommandHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    /// 删除一条历史；下标越界或库未变化时什么也不做。
    pub(super) fn delete_history_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.history.remove(index) {
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

    /// 插入 / 执行历史命令到当前活动会话。
    pub(super) fn send_history_command(&mut self, index: usize, run: bool, cx: &mut Context<Self>) {
        let Some(command) = self.history.entries().get(index).cloned() else {
            return;
        };
        if !self.send_command_to_terminal(&command, run, cx) {
            return;
        }
        let key = if run { "已执行历史命令" } else { "已插入历史命令" };
        self.status = crate::i18n::translate(cx, key, &[]);
        cx.notify();
    }

    /// 把历史命令复制进片段编辑器：名称默认取命令开头，备注标注来源。
    pub(super) fn save_history_as_snippet(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = self.history.entries().get(index).cloned() else {
            return;
        };
        if self.history_manager.take().is_none() {
            return;
        }
        self.open_snippet_manager_impl(window, cx);
        if let Some(manager) = self.snippet_manager.as_mut() {
            manager.builtin = false;
        }
        let name: String = command.chars().take(32).collect();
        self.begin_snippet_edit_values(None, &name, &command, "来自命令历史", window, cx);
        cx.notify();
    }
}
