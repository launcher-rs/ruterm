//! //! 底部状态栏：远程信息读数采集、条目拼装、显示项偏好与持久化。

use std::time::Duration;

use rgpui::components::status_bar::{StatusBar, StatusBarItem};
use rgpui::{App, Context, Entity, IconName, SharedString};
use rgpui_term::Terminal;

use crate::remote::Metric;
use crate::tr;

use super::*;

/// 活动终端的一帧读数，取自 rgpui-term 的渲染内容。
///
/// 只读内存里已经算好的 `last_content`（终端元素每帧 `sync` 过），不碰 PTY 也不碰网络；
/// 终端还没完成首次布局时列行都是 0，此时返回 `None`，不显示 `0×0`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StatusReadout {
    /// 列数。
    pub(super) cols: usize,
    /// 可视行数。
    pub(super) rows: usize,
    /// 光标行（1 基，含回滚偏移，即滚动到历史时也是画面上的行号）。
    pub(super) line: usize,
    /// 光标列（1 基）。
    pub(super) column: usize,
    /// 距底部滚过的行数。
    pub(super) scrolled: usize,
    /// 选区跨过的行数。
    pub(super) selected: usize,
}

impl StatusReadout {
    pub(super) fn capture(terminal: &Entity<Terminal>, cx: &App) -> Option<Self> {
        let content = terminal.read(cx).last_content();
        let bounds = content.terminal_bounds;
        if bounds.num_columns() == 0 || bounds.num_lines() == 0 {
            return None;
        }
        Some(Self {
            cols: bounds.num_columns(),
            rows: bounds.num_lines(),
            line: (content.cursor.point.line.0 + content.display_offset as i32).max(0) as usize + 1,
            column: content.cursor.point.column.0 + 1,
            scrolled: content.display_offset,
            selected: content
                .selection
                .as_ref()
                .map(|range| (range.end.line.0 - range.start.line.0).max(0) as usize + 1)
                .unwrap_or(0),
        })
    }
}

/// 反序列化缺字段时的默认：显示。
pub(super) fn shown() -> bool {
    true
}

/// 传输类瞬态提示：带图标 + 高亮背景，比左侧普通状态更醒目。
#[derive(Clone)]
pub(super) struct TransferFlash {
    pub(super) text: SharedString,
    pub(super) icon: IconName,
    /// 序号：过期定时器只清掉自己那一条。
    pub(super) seq: u64,
}

impl RuTermApp {
    /// 底部状态栏：左侧瞬态状态 + 活动会话信息（按用户勾选的显示项拼装），右侧会话计数。
    ///
    /// 可点的开关不放这里——状态栏两侧都被会话读数占着，会话一多按钮就被挤掉，
    /// 输入提示与同步输入的开关在「终端」菜单里。
    pub(super) fn render_status_bar(&self, cx: &Context<Self>) -> StatusBar {
        let active = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id));

        let mut left = Vec::new();
        if let Some(flash) = &self.transfer_flash {
            left.push(
                StatusBarItem::new(flash.text.clone())
                    .id("status")
                    .icon(flash.icon.clone())
                    .active(true),
            );
        } else {
            left.push(StatusBarItem::new(self.status.clone()).id("status"));
        }
        if let Some(tab) = active {
            if self.status_bar.addr {
                left.push(StatusBarItem::new(tab.addr.clone()).id("addr"));
            }
            // Telnet 没有认证方式这一栏，用协议名占住同一位置。
            if matches!(&tab.resume, Some(Resume::Telnet { .. })) {
                left.push(StatusBarItem::new("Telnet").id("protocol").muted(true));
            }
            if let (true, Some(auth)) = (self.status_bar.auth, tab.auth) {
                left.push(
                    StatusBarItem::new(auth_label(cx, auth))
                        .id("auth")
                        .muted(true),
                );
            }
            // 远程基本信息由 SSH 建立后的 exec 探测任务异步写入 tab.remote_info。
            if let Some(info) = tab.remote_info.as_ref() {
                for metric in Metric::ALL {
                    if !self.status_bar.metric_on(metric) {
                        continue;
                    }
                    // 远端没探到该项就整条不显示，不留空占位。
                    if let Some(value) = info.value(metric) {
                        // 发行版名自成一条，其余加标签；标签取词后才能跟着界面换语言。
                        let text = match metric {
                            Metric::Os => value.to_string(),
                            other => format!(
                                "{} {value}",
                                crate::i18n::translate(cx, other.label(), &[])
                            ),
                        };
                        left.push(StatusBarItem::new(SharedString::from(text)).id(metric.id()));
                    }
                }
            }
            // 终端读数取自终端上一帧同步好的渲染内容，本地读取、不碰 PTY。
            if self.status_bar.readout.any()
                && let Some(readout) = tab
                    .terminal_view
                    .as_ref()
                    .and_then(|view| StatusReadout::capture(view.read(cx).terminal(), cx))
            {
                left.extend(self.readout_items(readout, cx));
            }
        }
        let mut right = Vec::new();
        right.push(
            StatusBarItem::new(tr!(cx, "会话 {count}", count = self.tabs.len()))
                .id("sessions")
                .muted(true),
        );
        StatusBar::new().left(left).right(right)
    }

    /// 在状态栏打一条醒目的传输提示（图标 + 高亮背景）。
    /// `expire` 为 true 时数秒后自动淡出回普通 status；进行中提示不自动过期，
    /// 由完成时的新提示覆盖。
    pub(super) fn flash_transfer(
        &mut self,
        text: impl Into<SharedString>,
        icon: IconName,
        expire: bool,
        cx: &mut Context<Self>,
    ) {
        self.transfer_flash_seq += 1;
        let seq = self.transfer_flash_seq;
        self.transfer_flash = Some(TransferFlash {
            text: text.into(),
            icon,
            seq,
        });
        cx.notify();
        if !expire {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(6)).await;
            let _ = this.update(cx, |this, cx| {
                if this.transfer_flash.as_ref().is_some_and(|f| f.seq == seq) {
                    this.transfer_flash = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 终端读数条目：回滚与选区只在发生时出现，不显示 `回滚 0 行` 这种空占位。
    pub(super) fn readout_items(&self, readout: StatusReadout, cx: &App) -> Vec<StatusBarItem> {
        let prefs = &self.status_bar.readout;
        let mut items = Vec::new();
        if prefs.size {
            items.push(
                StatusBarItem::new(format!("{}×{}", readout.cols, readout.rows))
                    .id("readout-size")
                    .muted(true)
                    .tooltip(tr!(
                        cx,
                        "终端尺寸：{cols} 列 × {rows} 行",
                        cols = readout.cols,
                        rows = readout.rows
                    )),
            );
        }
        if prefs.cursor {
            items.push(
                StatusBarItem::new(tr!(
                    cx,
                    "行 {line} 列 {column}",
                    line = readout.line,
                    column = readout.column
                ))
                .id("readout-cursor")
                .muted(true)
                .tooltip(tr!(cx, "光标位置")),
            );
        }
        if prefs.scrollback && readout.scrolled > 0 {
            items.push(
                StatusBarItem::new(tr!(cx, "回滚 {lines} 行", lines = readout.scrolled))
                    .id("readout-scrollback")
                    .muted(true)
                    .tooltip(tr!(
                        cx,
                        "已向上滚动 {lines} 行，到底部时不再显示",
                        lines = readout.scrolled
                    )),
            );
        }
        if prefs.selection && readout.selected > 0 {
            items.push(
                StatusBarItem::new(tr!(cx, "选区 {lines} 行", lines = readout.selected))
                    .id("readout-selection")
                    .tooltip(tr!(cx, "选中的行数")),
            );
        }
        items
    }

    /// 状态栏终端读数的刷新循环。
    ///
    /// 终端的渲染内容只在元素绘制时 `sync`，而 `Wakeup` / `SelectionsChanged` 这些事件
    /// 发在 sync 之前，事件回调里读到的是上一帧，光靠订阅会把读数钉死在旧值上。
    /// 因此按 250ms 轮询活动终端的当前内容，读数没变就不触发重绘。
    pub(super) fn watch_readout(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let Ok(()) = this.update(cx, |this, cx| {
                    // 命令历史与读数同一拍刷新：block 只在终端 sync 后才出现，
                    // 这里扫到新 block 才重绘，没变化时开销只是几次空遍历。
                    this.sync_command_history(cx);
                    let active = this.active_tab;
                    let next = active
                        .and_then(|id| this.tabs.iter().find(|tab| tab.id == id))
                        .and_then(|tab| {
                            tab.terminal_view.as_ref().and_then(|view| {
                                StatusReadout::capture(view.read(cx).terminal(), cx)
                            })
                        });
                    if next == this.readout_seen {
                        return;
                    }
                    this.readout_seen = next;
                    cx.notify();
                }) else {
                    return;
                };
            }
        })
        .detach();
    }

    /// 勾选状态栏显示项：即时生效并持久化（库内优先，降级时写 settings.json）。
    pub(super) fn set_status_pref(&mut self, pref: StatusPref, on: bool, cx: &mut Context<Self>) {
        match pref {
            StatusPref::Addr => self.status_bar.addr = on,
            StatusPref::Auth => self.status_bar.auth = on,
            StatusPref::Metric(metric) => self.status_bar.set_metric(metric, on),
            StatusPref::Readout(key) => self.status_bar.readout.set(key, on),
        }
        cx.notify();
        self.persist_status_bar(cx);
    }

    /// 状态栏偏好落库；加密库不可用时退回旧 settings.json。
    pub(super) fn persist_status_bar(&self, cx: &mut Context<Self>) {
        let json = self.status_bar.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_status_bar(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("status_bar", &json).await {
                log::warn!("写入库内状态栏偏好失败: {error}");
            }
        })
        .detach();
    }
}
