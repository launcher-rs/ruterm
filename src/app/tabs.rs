//! //! 会话标签：生命周期（结束 / 失败 / 重连 / 移除）、重命名、分屏、托盘同步与标签渲染。

use std::time::{Duration, Instant};

use rgpui::components::split_pane::{SplitDirection, SplitPaneState};
use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, ClipboardItem, Context, Disableable, Entity,
    ExternalPaths, Focusable, FontWeight, Hsla, Icon, IconName, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TrayMenuItem, Window, div, h_flex, px, v_flex,
};
use rgpui_term::{
    Event as TerminalEvent, SearchStatus, Terminal, TerminalView,
};

use crate::config::SshTarget;
use crate::desktop::Command;
use crate::remote::RemoteInfo;
use crate::sessions::{AuthMethod, HostConfig, ProtocolKind};
use crate::{CancelRename, CloseTab, SplitRight, SplitDown, CloseSplit, NextTab, PrevTab};
use crate::tr;

use super::*;

/// 一个已连接的会话标签（持有终端视图）。
pub(super) struct SessionTab {
    /// 稳定标签 id（会话结束或用户关闭时据此定位）。
    pub(super) id: usize,
    /// 标签标题：当前实际显示的那一个（自定义 > 终端 OSC 标题 > 默认名）。
    pub(super) title: SharedString,
    /// 默认标题（主机名 / 本地 shell 名）：终端没报标题、或用户撤销重命名时回落它。
    pub(super) base_title: SharedString,
    /// 用户双击重命名的标题：优先级最高，OSC 标题不再覆盖它。
    pub(super) custom_title: Option<SharedString>,
    /// 连接位置（user@host:port 或本地 shell 的路径），状态栏展示。
    pub(super) addr: SharedString,
    /// 认证方式，状态栏展示；本地终端没有认证概念，为 None。
    pub(super) auth: Option<AuthMethod>,
    /// 远程服务器基本信息（连接后经 exec 探测异步填充，探测失败保持 None）。
    pub(super) remote_info: Option<RemoteInfo>,
    /// 渲染用终端视图；远程桌面标签为 None。
    pub(super) terminal_view: Option<Entity<TerminalView>>,
    /// 远程桌面标签的状态（连接表单 / 帧 / 输入转发）；终端标签为 None。
    pub(super) desktop: Option<DesktopTab>,
    /// 会话已结束（远端 exit / 断线 / 连接失败）。标签保留终端内容并显示重连条。
    pub(super) ended: bool,
    /// 结束原因是失败（连接失败 / 异常中断）：状态栏文案由失败路径负责，不被"已断开"覆盖。
    pub(super) failed: bool,
    /// 失败原文（russh / IO 错误串）：横幅就地显示摘要并据此「复制错误」，重连成功后清空。
    pub(super) last_error: Option<SharedString>,
    /// 重连所需参数；本地终端没有会话概念，为 None。
    pub(super) resume: Option<Resume>,
    /// 主动断开当前 SSH 会话（关闭标签时发送；本地终端为 None）。
    pub(super) disconnect: Option<tokio::sync::oneshot::Sender<()>>,
    /// 会话指令通道（SFTP 传输 / 端口转发；本地终端为 None）。
    pub(super) commands: Option<tokio::sync::mpsc::UnboundedSender<crate::session::SessionCommand>>,
    /// 本标签当前生效的端口转发规则（随会话结束清空，不持久化）。
    pub(super) forwards: Vec<crate::session::ForwardRule>,
    /// 右侧文件面板的浏览状态（路径 / 列表 / 加载态），按标签各自保留。
    pub(super) files: FileBrowser,
    /// 终端当前工作目录：由 SSH 桥接层经 cd 跟踪 / OSC 7 上报确认后回传，
    /// 文件面板"跟随终端"开关据此跳转；本地终端无此概念，为 None。
    pub(super) terminal_cwd: Option<String>,
    /// 拖文件到终端（lrzsz）的当前操作文案；None 表示空闲。
    pub(super) zmodem: Option<SharedString>,
    /// lrzsz 上传进度 (已传, 总量)。
    pub(super) zmodem_progress: Option<(u64, u64)>,
    /// 本轮连续自动重连已尝试次数（0＝不在自动重连中，手动重连会清零）。
    pub(super) auto_retry_attempt: u32,
    /// 已排定一次自动重连（横幅展示等待与取消入口）。
    pub(super) auto_retry_pending: bool,
    /// 本次会话 shell 成功开启的时刻：稳定运行过一阵后再断线，自动重连次数重新计。
    pub(super) connected_at: Option<Instant>,
}

/// 分屏视图：主区同时显示两个会话标签（左/上＝first，右/下＝second）。
///
/// 只把"哪两个标签并排"记成一个固定有序对：点哪一侧就切到哪个标签，
/// 但两屏位置不互换；从标签栏切到第三个会话时自动退出分屏。
pub(super) struct SplitView {
    pub(super) first: usize,
    pub(super) second: usize,
    pub(super) direction: SplitDirection,
    /// 可拖拽的分隔条状态（比例只存活在内存，每次分屏从 50% 开始）。
    pub(super) pane: Entity<SplitPaneState>,
}

/// 断线标签的重连参数快照。
#[derive(Clone)]
pub(super) enum Resume {
    /// SSH 会话。
    Ssh {
        /// 原连接目标（含仅内存持有的密码/口令，绝不落盘）。
        target: SshTarget,
        /// 来源主机（None＝命令行快速连接）；重连时优先用列表里的最新配置。
        host: Option<HostConfig>,
        /// 秘密是否取自加密库：重连仍失败时据此清掉库里的坏凭据。
        secret_from_vault: bool,
    },
    /// Telnet 会话（明文 TCP，无秘密）。
    Telnet {
        /// 地址与端口：主机被删或改成 SSH 时仍能按原地址重连。
        host: String,
        port: u16,
        /// 来源主机：重连时优先用列表里的最新地址 / 端口。
        source: Option<HostConfig>,
    },
}

impl RuTermApp {
    /// 标签仍属于当前这次连接（未被同 id 的新会话替换/移除）。
    pub(super) fn tab_is_current(&self, id: usize, view: &Entity<TerminalView>) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.id == id && tab.terminal_view.as_ref() == Some(view))
    }


    /// 终端报告会话结束：保留终端内容与标签，展示断开横幅（可重连）。
    pub(super) fn end_session_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        if tab.ended {
            return;
        }
        tab.ended = true;
        let had_forwards = !tab.forwards.is_empty();
        // 端口转发随会话进程一起停：本地监听端口已释放，列表不能继续展示"生效中"。
        tab.forwards.clear();
        let (title, failed, active) = (tab.title.clone(), tab.failed, self.active_tab == Some(id));
        if active && !failed {
            self.status =
                SharedString::from(format!("会话已断开 {title}，按回车或点击「重新连接」恢复"));
        }
        if had_forwards
            && let Some(dialog) = self.forward.as_mut().filter(|dialog| dialog.tab_id == id)
        {
            dialog.info = None;
            dialog.error = Some("会话已断开，端口转发已全部停止".into());
        }
        self.prune_multi_session_state();
        cx.notify();
    }


    /// 会话减少后收拾依赖多个会话的状态：可用会话不足两个就关掉同步输入，
    /// 一个都不剩就收起广播弹窗（继续留着只会对着空列表发命令）。
    pub(super) fn prune_multi_session_state(&mut self) {
        let live = self.live_tab_count();
        if self.sync_input && live < 2 {
            self.sync_input = false;
            if self.status.as_ref().starts_with("同步输入") {
                self.status = "同步输入已关闭（可用会话不足两个）".into();
            }
        }
        if live == 0 {
            self.broadcast = None;
        }
    }


    /// 会话以失败收场：标记断开原因，状态栏给出错误摘要（终端里另有完整错误文本）。
    ///
    /// 网络类失败按设置排定自动重连（退避 + 次数上限）；首次连接就失败不自动重试，
    /// 避免把写错的地址反复重连。
    pub(super) fn fail_session_tab(
        &mut self,
        id: usize,
        message: SharedString,
        shell_started: bool,
        cx: &mut Context<Self>,
    ) {
        let mut schedule = None;
        let mut had_forwards = false;
        let auto_enabled = self.session_prefs.auto_reconnect;
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            tab.failed = true;
            // 失败也按"已结束"落标签：终端事件可能迟到，横幅与重连入口要立刻可用。
            tab.ended = true;
            tab.last_error = Some(message.clone());
            had_forwards = !tab.forwards.is_empty();
            // 连接没了，本地监听端口随会话任务一起释放。
            tab.forwards.clear();
            // 稳定跑过一阵的会话：这是新一轮断线，重试次数清零。
            if tab
                .connected_at
                .is_some_and(|at| at.elapsed() >= AUTO_RECONNECT_HEALTHY)
            {
                tab.auto_retry_attempt = 0;
            }
            tab.connected_at = None;
            if self.active_tab == Some(id) {
                self.status = message;
            }
            let in_streak = tab.auto_retry_attempt > 0;
            if auto_enabled
                && tab.resume.is_some()
                && (shell_started || in_streak)
                && tab.auto_retry_attempt < AUTO_RECONNECT_MAX
                && let Some(view) = tab.terminal_view.clone()
            {
                let attempt = tab.auto_retry_attempt + 1;
                let delay = auto_reconnect_delay(attempt);
                tab.auto_retry_attempt = attempt;
                tab.auto_retry_pending = true;
                schedule = Some((attempt, delay, view));
            }
            cx.notify();
        }
        if had_forwards
            && let Some(dialog) = self.forward.as_mut().filter(|dialog| dialog.tab_id == id)
        {
            dialog.info = None;
            dialog.error = Some("连接已断开，端口转发已全部停止".into());
        }
        if let Some((attempt, delay, view)) = schedule {
            if self.active_tab == Some(id) {
                self.status = SharedString::from(format!(
                    "连接中断，{} 秒后自动重连（第 {attempt} 次，共 {AUTO_RECONNECT_MAX} 次）",
                    delay.as_secs()
                ));
            }
            self.schedule_auto_reconnect(id, view, attempt, delay, cx);
        }
        // 失败同样让该会话退出"可用"计数：不足两个就关掉同步输入。
        self.prune_multi_session_state();
    }


    /// 延迟到点后在原标签重连；期间关标签、手动重连、取消或关掉开关都会让本次排定作废。
    pub(super) fn schedule_auto_reconnect(
        &mut self,
        id: usize,
        view: Entity<TerminalView>,
        attempt: u32,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(delay).await;
            let _ = cx.with_window(entity_id, move |window, cx| {
                entity.update(cx, move |this, cx| {
                    let still_pending = this.tabs.iter().any(|tab| {
                        tab.id == id
                            && tab.terminal_view.as_ref() == Some(&view)
                            && tab.auto_retry_pending
                            && tab.ended
                    });
                    if !still_pending || !this.session_prefs.auto_reconnect {
                        if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == id) {
                            tab.auto_retry_pending = false;
                            cx.notify();
                        }
                        return;
                    }
                    this.reconnect_tab_with(id, attempt, window, cx);
                });
            });
        })
        .detach();
    }


    /// 取消本轮自动重连：已排定的那次作废，等待用户按回车或点「重新连接」。
    pub(super) fn cancel_auto_reconnect(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            tab.auto_retry_pending = false;
            tab.auto_retry_attempt = AUTO_RECONNECT_MAX;
            cx.notify();
        }
        self.status = "已取消自动重连，按回车或点击「重新连接」手动恢复".into();
    }


    /// 手动重连一个已断开的标签（重置自动重连计数）。
    pub(super) fn reconnect_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.reconnect_tab_with(id, 0, window, cx);
    }


    /// 重连一个已断开的标签：优先用列表里的最新主机配置，其余沿用原连接参数。
    /// `retry_attempt` 为自动重连的连续次数（手动重连传 0）。
    pub(super) fn reconnect_tab_with(
        &mut self,
        id: usize,
        retry_attempt: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((title, resume)) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id && tab.ended)
            .and_then(|tab| {
                tab.resume
                    .clone()
                    .map(|resume| (tab.title.to_string(), resume))
            })
        else {
            return;
        };
        match resume {
            Resume::Ssh {
                target: old_target,
                host: old_host,
                secret_from_vault,
            } => {
                let current_host = old_host
                    .as_ref()
                    .and_then(|host| self.hosts.iter().find(|item| item.id == host.id));
                let (target, host, cache_key) = match current_host {
                    Some(host) => {
                        // 主机可能被编辑过：地址/端口/用户/认证方式以最新配置为准，
                        // 秘密沿用原会话（仅内存），避免重连时再弹一次窗。
                        let password = (host.auth == AuthMethod::Password)
                            .then(|| old_target.password.clone())
                            .flatten();
                        let Ok(mut target) = self.host_target(host, password) else {
                            self.status =
                                "内嵌私钥不可用（加密库未解锁或数据损坏），无法重连".into();
                            cx.notify();
                            return;
                        };
                        if host.auth == AuthMethod::IdentityFile {
                            target.passphrase = old_target.passphrase.clone();
                        }
                        (target, Some(host.clone()), Some(host.addr()))
                    }
                    None => (old_target, old_host, None),
                };
                self.start_ssh(
                    id,
                    title,
                    target,
                    cache_key,
                    host,
                    secret_from_vault,
                    retry_attempt,
                    true,
                    window,
                    cx,
                );
            }
            Resume::Telnet { host, port, source } => {
                // 主机可能被编辑过：仍为 Telnet 时按最新地址 / 端口重连；
                // 已改成 SSH 或已删除则沿用原地址。
                let latest = source
                    .as_ref()
                    .and_then(|saved| self.hosts.iter().find(|item| item.id == saved.id).cloned());
                let (host, port, source) = match latest {
                    Some(latest) if latest.protocol == ProtocolKind::Telnet => {
                        (latest.host.clone(), latest.port, Some(latest))
                    }
                    _ => (host, port, source),
                };
                self.start_telnet(
                    id,
                    title,
                    host,
                    port,
                    source,
                    retry_attempt,
                    true,
                    window,
                    cx,
                );
            }
        }
    }


    /// 用户点击标签关闭按钮。
    /// 订阅终端事件：查找状态变了要重绘查找条，OSC 0/2 标题变了要改标签名。
    pub(super) fn watch_terminal(&self, tab_id: usize, terminal: &Entity<Terminal>, cx: &mut Context<Self>) {
        cx.subscribe(terminal, move |this, terminal, event: &TerminalEvent, cx| {
            match event {
                TerminalEvent::SearchUpdated(_) => cx.notify(),
                TerminalEvent::TitleChanged => {
                    let title = terminal.read(cx).breadcrumb_text.clone();
                    this.apply_osc_title(tab_id, &title, cx);
                }
                _ => {}
            }
        })
        .detach();
    }


    /// 把终端上报的 OSC 标题落到标签上；用户重命名过就不再覆盖。
    pub(super) fn apply_osc_title(&mut self, tab_id: usize, title: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.custom_title.is_some() {
            return;
        }
        // 终端清标题（ResetTitle 给空串）时回落默认名，别让标签空着。
        tab.title = match title.trim() {
            "" => tab.base_title.clone(),
            text => SharedString::from(text.to_string()),
        };
        cx.notify();
    }


    /// 双击标签：把标题换成输入框就地改名。只影响这条会话，不落库。
    pub(super) fn begin_rename(
        &mut self,
        tab_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return;
        };
        let initial = tab.title.to_string();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
            // 输入框可能已随上一次提交销毁，只认当前挂着的那个。
            if !this
                .renaming
                .as_ref()
                .is_some_and(|(_, state)| state == &input)
            {
                return;
            }
            match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_rename(cx),
                _ => {}
            }
        })
        .detach();
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            let len = state.value().len();
            state.set_selected_range(0..len, cx);
        });
        self.renaming = Some((tab_id, input));
        cx.notify();
    }

    /// 提交就地改名：空标题当作撤销，交还给终端上报的标题或默认名。
    pub(super) fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some((tab_id, input)) = self.renaming.take() else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            cx.notify();
            return;
        };
        if text.is_empty() {
            self.tabs[index].custom_title = None;
            let osc = self.tabs[index]
                .terminal_view
                .as_ref()
                .map(|view| view.read(cx).terminal().read(cx).breadcrumb_text.clone())
                .unwrap_or_default();
            self.apply_osc_title(tab_id, &osc, cx);
        } else {
            let title = SharedString::from(text);
            self.tabs[index].custom_title = Some(title.clone());
            self.tabs[index].title = title;
        }
        cx.notify();
    }


    pub(super) fn cancel_rename(&mut self, _: &CancelRename, _: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            cx.notify();
        }
    }


    pub(super) fn close_active_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_tab else {
            return;
        };
        self.close_tab(id, cx);
    }


    pub(super) fn close_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        self.remove_tab(id, cx);
    }


    /// 移除标签并修正选中态；顺带把状态栏切到"已断开"。
    pub(super) fn remove_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        let title = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.title.clone());
        let before = self.tabs.len();
        // 关标签＝主动断开：通知会话任务释放 SSH 连接，而不是留一条后台挂着的连接。
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id)
            && let Some(disconnect) = tab.disconnect.take()
        {
            let _ = disconnect.send(());
        }
        // 远程桌面标签：通知协议层收尾，并把残留帧从图集里释放掉。
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id)
            && let Some(desktop) = tab.desktop.as_mut()
        {
            desktop.send(Command::Close);
            let mut frames: Vec<_> = desktop.drop_queue.drain(..).collect();
            frames.extend(desktop.image.take());
            for image in frames {
                cx.drop_image(image, None);
            }
        }
        self.tabs.retain(|tab| tab.id != id);
        if self.tabs.len() == before {
            return;
        }
        // 分屏里的任一侧没了（关标签 / 本地 shell 退出）就回到单屏。
        if self
            .view_split
            .as_ref()
            .is_some_and(|view| view.first == id || view.second == id)
        {
            self.view_split = None;
        }
        // 转发表单绑定的是这条连接：标签没了就一起关掉，避免对着死会话操作。
        if self
            .forward
            .as_ref()
            .is_some_and(|dialog| dialog.tab_id == id)
        {
            self.forward = None;
        }
        if let Some(title) = title {
            self.status = SharedString::from(format!("已断开 {title}"));
        }
        if self.active_tab == Some(id) {
            self.active_tab = self.tabs.last().map(|tab| tab.id);
            if let Some(active) = self.active_tab {
                self.reveal_tab(active);
            }
        }
        self.prune_multi_session_state();
        self.sync_tray(cx);
        cx.notify();
    }


    /// 切到指定标签：选中、滚进可见范围（标签多到溢出时），并把焦点交给它的终端。
    pub(crate) fn activate_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        // 分屏只描述"哪两个标签并排"：从标签栏 / 快捷键切到第三个会话时退出分屏，
        // 点两个分屏面板之一则保持在分屏里（两屏位置不互换）。
        if let Some(view) = self.view_split.as_ref()
            && view.first != id
            && view.second != id
        {
            self.view_split = None;
            self.status = "已退出分屏".into();
        }
        self.active_tab = Some(id);
        // 边栏停留在文件页时切到尚未浏览过的新会话，顺手列出登录主目录。
        let needs_files = self.sidebar_visible(cx)
            && self.sidebar_page == SidebarPage::Files
            && self.tabs.iter().any(|tab| {
                tab.id == id
                    && tab.files.path.is_none()
                    && tab.commands.is_some()
                    && !tab.ended
                    && !tab.files.loading
            });
        if needs_files {
            self.load_files(id, None, cx);
        }
        // 转发表单绑定单个会话：切到别的标签时重开该会话的面板，避免表单指向旧连接。
        if self.sidebar_visible(cx)
            && self.sidebar_page == SidebarPage::Forward
            && self
                .forward
                .as_ref()
                .is_some_and(|dialog| dialog.tab_id != id)
        {
            self.forward = None;
            self.open_forward(window, cx);
        }
        if let Some(ix) = self.tabs.iter().position(|tab| tab.id == id) {
            self.tab_scroll.scroll_to_item(ix);
            if let Some(view) = self.tabs[ix].terminal_view.clone() {
                let focus = view.read(cx).focus_handle(cx);
                focus.focus(window, cx);
            } else if let Some(desktop) = self.tabs[ix].desktop.as_ref() {
                // 切回桌面标签时补一次整帧：后台期间可能漏掉了增量更新。
                if matches!(desktop.phase, DesktopPhase::Live) {
                    desktop.send(Command::RequestFull);
                }
                let focus = desktop.focus.clone();
                focus.focus(window, cx);
            }
        }
        // 查找条只作用于活动终端：切标签后把同一关键词带到新终端并跳首个匹配。
        if self.find.visible {
            self.find.applied = None;
            self.apply_find(cx);
        }
        self.sync_tray(cx);
    }


    /// Ctrl+Shift+D / 菜单项：左右分屏；已在左右分屏时再按一次即取消。
    pub(super) fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_split(SplitDirection::Horizontal, window, cx);
    }


    /// Ctrl+Shift+E / 菜单项：上下分屏；已在上下分屏时再按一次即取消。
    pub(super) fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_split(SplitDirection::Vertical, window, cx);
    }


    pub(super) fn close_split_action(&mut self, _: &CloseSplit, _: &mut Window, cx: &mut Context<Self>) {
        self.close_split("已取消分屏", cx);
    }


    pub(super) fn close_split(&mut self, status: &str, cx: &mut Context<Self>) {
        if self.view_split.take().is_some() {
            self.status = SharedString::from(status.to_string());
            cx.notify();
        }
    }


    /// 分屏开关：同方向＝取消；换方向＝原地换排布；未分屏时把活动标签与下一个标签并排。
    pub(super) fn toggle_split(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.view_split.as_mut() {
            if view.direction == direction {
                self.close_split("已取消分屏", cx);
                return;
            }
            view.direction = direction;
            let pane = view.pane.clone();
            pane.update(cx, |state, cx| state.set_direction(direction, cx));
            self.status = SharedString::from(if direction == SplitDirection::Horizontal {
                "已切换为左右分屏"
            } else {
                "已切换为上下分屏"
            });
            cx.notify();
            return;
        }
        let Some(active) = self
            .active_tab
            .filter(|id| self.tabs.iter().any(|tab| tab.id == *id))
            .or_else(|| self.tabs.last().map(|tab| tab.id))
        else {
            self.status = "没有可分屏的会话".into();
            cx.notify();
            return;
        };
        // 伙伴取活动标签的下一个（末尾则取上一个）；只有一个标签时没法分屏。
        let Some(second) = self
            .tabs
            .iter()
            .position(|tab| tab.id == active)
            .and_then(|ix| {
                let next = self.tabs.get(ix + 1).map(|tab| tab.id);
                next.or_else(|| ix.checked_sub(1).map(|prev| self.tabs[prev].id))
            })
        else {
            self.status = "分屏需要两个标签：先再开一个会话".into();
            cx.notify();
            return;
        };
        let pane = cx.new(|cx| {
            let mut state = SplitPaneState::new(cx);
            state.set_direction(direction, cx);
            // 两边都留出能看几行的宽度，拖到极小意义不大。
            state.set_min_first(240.0, cx);
            state.set_min_second(240.0, cx);
            state
        });
        // 左/上取标签顺序靠前的那个，两屏位置与标签栏顺序一致。
        let active_ix = self
            .tabs
            .iter()
            .position(|tab| tab.id == active)
            .unwrap_or(0);
        let second_ix = self
            .tabs
            .iter()
            .position(|tab| tab.id == second)
            .unwrap_or(0);
        let (first, second) = if active_ix < second_ix {
            (active, second)
        } else {
            (second, active)
        };
        self.view_split = Some(SplitView {
            first,
            second,
            direction,
            pane,
        });
        self.activate_tab(active, window, cx);
        self.status = SharedString::from(if direction == SplitDirection::Horizontal {
            "已左右分屏（拖中间分隔条调宽度）"
        } else {
            "已上下分屏（拖中间分隔条调高度）"
        });
        cx.notify();
    }


    /// 把当前会话列表刷到托盘菜单与悬停提示（隐藏到托盘后也能直接跳回某个标签）。
    pub(crate) fn sync_tray(&self, cx: &mut Context<Self>) {
        let mut menu = Vec::with_capacity(self.tabs.len() + 4);
        for tab in &self.tabs {
            let marker = if Some(tab.id) == self.active_tab {
                "●"
            } else {
                "○"
            };
            menu.push(TrayMenuItem::Action {
                label: format!("{marker} {}", tab.title).into(),
                id: format!("tab:{}", tab.id).into(),
            });
        }
        if !menu.is_empty() {
            menu.push(TrayMenuItem::Separator);
        }
        menu.push(TrayMenuItem::Action {
            label: tr!(cx, "显示主界面"),
            id: "show".into(),
        });
        menu.push(TrayMenuItem::Separator);
        menu.push(TrayMenuItem::Action {
            label: tr!(cx, "退出"),
            id: "quit".into(),
        });
        cx.set_tray_menu(menu);
        let tooltip = if self.tabs.is_empty() {
            tr!(cx, "RuTerm 拾连")
        } else {
            tr!(cx, "RuTerm 拾连 · {count} 个会话", count = self.tabs.len())
        };
        cx.set_tray_tooltip(tooltip.as_str());
    }


    /// 让标签栏滚到能看见指定标签（不改变选中态）。
    pub(super) fn reveal_tab(&self, id: usize) {
        if let Some(ix) = self.tabs.iter().position(|tab| tab.id == id) {
            self.tab_scroll.scroll_to_item(ix);
        }
    }


    /// 拖拽排序标签：把第 `from` 个标签移到第 `to` 个标签的位置。
    /// 先取出再插入，`from < to` 时目标下标会左移一位，这里用移出后的下标插入，
    /// 视觉上落点标签让位、被拖标签占住它的位置（与 rgpui 自带 TabBar 同规则）。
    pub(super) fn reorder_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        if from >= len {
            return;
        }
        let to = to.min(len.saturating_sub(1));
        if from == to {
            return;
        }
        let tab = self.tabs.remove(from);
        let insert_at = to.min(self.tabs.len());
        self.tabs.insert(insert_at, tab);
        cx.notify();
    }


    /// Ctrl+Tab / Ctrl+Shift+Tab：按标签顺序循环切换（末尾回到开头）。
    pub(super) fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let current = self
            .active_tab
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
            .unwrap_or(0);
        let next = (current as isize + step).rem_euclid(self.tabs.len() as isize) as usize;
        let id = self.tabs[next].id;
        self.activate_tab(id, window, cx);
        cx.notify();
    }


    pub(super) fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, window, cx);
    }


    pub(super) fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(-1, window, cx);
    }


    /// 主区里的一个会话面板：终端内容 + 内边距 + 断开横幅。
    ///
    /// `highlight` 只在分屏时给活动一侧加一圈强调色边框——单屏不需要。
    pub(super) fn render_tab_pane(
        &self,
        tab: &SessionTab,
        active: bool,
        highlight: bool,
        terminal_bg: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let id = tab.id;
        let desktop = tab.desktop.is_some();
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(terminal_bg)
            .when(!desktop, |this| this.px_3().py_2())
            .when(highlight && active, |this| {
                this.border_1().border_color(pal.accent)
            })
            // 分屏时点一下另一侧即把焦点/状态栏切过去；点击落在终端上也会冒泡到这里。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if this.active_tab != Some(id) {
                        this.activate_tab(id, window, cx);
                        cx.notify();
                    }
                }),
            )
            // 本地文件拖到终端＝用 lrzsz 传到 shell 当前目录（落点由远端决定）。
            // 远程桌面标签没有 shell，不接这个落点。
            .when(!desktop, |this| {
                this.drag_over::<ExternalPaths>(move |style, _, _, _| {
                    style.border_1().border_color(pal.accent)
                })
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                    this.upload_via_terminal(id, paths.paths().to_vec(), cx);
                }))
            })
            .when(active && self.find.visible && !desktop, |this| {
                this.child(self.render_find_bar(tab, cx))
            })
            .when_some(tab.terminal_view.clone(), |this, view| {
                this.child(view)
            })
            .when_some(tab.desktop.as_ref(), |this, desktop| {
                this.child(self.render_desktop_pane(tab, desktop, cx))
            })
            // 远程桌面有自己的失败横幅（含重连），不叠一层终端横幅。
            .when(tab.ended && tab.desktop.is_none(), |this| {
                this.child(self.render_ended_banner(tab, cx))
            })
            .when_some(tab.zmodem.clone(), |this, text| {
                let (done, total) = tab.zmodem_progress.unwrap_or((0, 0));
                let fraction = if total > 0 {
                    (done as f32 / total as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                this.child(
                    v_flex()
                        .absolute()
                        .top(px(10.0))
                        .right(px(10.0))
                        .w(px(260.0))
                        .gap_1()
                        .px_3()
                        .py_2()
                        .rounded(px(6.0))
                        .bg(pal.card_bg)
                        .border_1()
                        .border_color(pal.border)
                        .shadow_md()
                        .child(div().text_xs().text_color(pal.text).child(if total > 0 {
                            format!(
                                "{text} {} / {}",
                                crate::transfer::format_bytes(done),
                                crate::transfer::format_bytes(total)
                            )
                        } else {
                            text.to_string()
                        }))
                        .child(
                            div()
                                .h(px(4.0))
                                .w_full()
                                .rounded(px(2.0))
                                .bg(pal.border)
                                .overflow_hidden()
                                .child(
                                    div()
                                        .h_full()
                                        .w(rgpui::relative(fraction))
                                        .rounded(px(2.0))
                                        .bg(pal.accent),
                                ),
                        ),
                )
            })
            .into_any_element()
    }

    /// 终端内查找条：关键词输入 + 匹配状态 + 上/下跳转 + 正则开关。
    ///
    /// 不显示「第几个 / 共几个」——rgpui-term 只给匹配状态、不给总数，
    /// 所以用状态文案（正则无效 / 未找到）代替计数。
    pub(super) fn render_find_bar(&self, tab: &SessionTab, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(view) = tab.terminal_view.as_ref() else {
            return div().into_any_element();
        };
        let status = view.read(cx).search_status(cx);
        let (hint, hint_color) = match status {
            SearchStatus::Invalid => ("正则无效", pal.danger),
            SearchStatus::NotFound => ("未找到", pal.muted),
            _ => ("", pal.muted),
        };
        let steppable = status == SearchStatus::Matched;
        let regex = self.find.regex;
        h_flex()
            .id("find-bar")
            .key_context("FindBar")
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .mb_2()
            .px_2()
            .py_1()
            .rounded(px(6.0))
            .bg(pal.card_bg)
            .border_1()
            .border_color(pal.border)
            .child(Icon::new(IconName::Search).small().text_color(pal.muted))
            .child(
                div()
                    .w(px(260.0))
                    .child(styled_input(Input::new(&self.find.input).text_xs(), &pal)),
            )
            .child(div().flex_1().text_xs().text_color(hint_color).child(hint))
            .child(
                Button::new("find-prev")
                    .icon(IconName::ArrowUp)
                    .compact()
                    .ghost()
                    .tab_index(-1)
                    .tooltip("上一个匹配（Shift+回车）")
                    .disabled(!steppable)
                    .on_click(cx.listener(|this, _, _, cx| this.step_find(true, cx))),
            )
            .child(
                Button::new("find-next")
                    .icon(IconName::ArrowDown)
                    .compact()
                    .ghost()
                    .tab_index(-1)
                    .tooltip("下一个匹配（回车）")
                    .disabled(!steppable)
                    .on_click(cx.listener(|this, _, _, cx| this.step_find(false, cx))),
            )
            .child(
                Button::new("find-regex")
                    .icon(IconName::CaseSensitive)
                    .compact()
                    .ghost()
                    .tab_index(-1)
                    .selected(regex)
                    .tooltip(if regex {
                        "正则匹配（点击改为按字面量）"
                    } else {
                        "按字面量匹配（点击启用正则）"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.find.regex = !this.find.regex;
                        this.find.applied = None;
                        this.apply_find(cx);
                    })),
            )
            .child(
                Button::new("find-close")
                    .icon(IconName::Close)
                    .compact()
                    .ghost()
                    .tab_index(-1)
                    .tooltip("关闭查找（Esc）")
                    .on_click(cx.listener(|this, _, window, cx| this.do_close_find(window, cx))),
            )
            .into_any_element()
    }


    /// 会话断开横幅：浮在终端顶部，说明连接已结束并提供重连入口。
    ///
    /// 标签不自动关闭（与 WindTerm 一致）：终端内容留在原处可回看，按回车
    /// 或点「重新连接」在原标签内重建会话；开启了自动重连时先按退避等待并显示
    /// 倒计时，可随时取消。凭据已被服务器拒绝时不提供重连（重试同一份秘密只会
    /// 再失败一次），提示回到列表重新点击。
    pub(super) fn render_ended_banner(&self, tab: &SessionTab, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let theme = cx.theme();
        let fg = theme.warning_foreground;
        let id = tab.id;
        let reconnectable = tab.resume.is_some();
        let attempt = tab.auto_retry_attempt;
        let pending = tab.auto_retry_pending && reconnectable;
        let exhausted = reconnectable && !pending && attempt >= AUTO_RECONNECT_MAX;
        let hint = if pending {
            tr!(
                cx,
                "正在自动重连（第 {attempt} 次，{secs} 秒后）",
                attempt = attempt,
                secs = auto_reconnect_delay(attempt).as_secs()
            )
        } else if exhausted {
            tr!(cx, "自动重连已停止，按回车手动重连")
        } else if reconnectable {
            tr!(cx, "按回车重新连接")
        } else {
            tr!(cx, "请从左侧重新点击该主机连接")
        };
        let banner_title = tr!(cx, "远程通道已关闭");
        let reason = if tab.failed {
            tr!(cx, "连接中断。")
        } else {
            tr!(cx, "会话已断开连接。")
        };
        // 错误原文压成一行（换行只当空格）；太长仍要截断，完整内容靠「复制错误」进剪贴板。
        let error_line = tab
            .last_error
            .as_ref()
            .map(|err| flatten_error_text(err.as_ref(), 220));
        let copy_error_label = tr!(cx, "复制错误");
        let copy_error_tip = tr!(cx, "把断开原因原文复制到剪贴板，方便贴给别人排查");
        let copied_status = tr!(cx, "错误已复制到剪贴板");
        let error_text = tab.last_error.clone();
        let cancel_retry_label = tr!(cx, "取消自动重连");
        let reconnect_label = tr!(cx, "重新连接");
        // 一行装不下：标题/入口在第一行，错误原文单独一行，否则长错误会把按钮挤出窗口。
        let head_row = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight(600.0))
                    .child(banner_title),
            )
            .child(div().text_sm().child(reason))
            .child(div().flex_1())
            .child(div().text_xs().child(hint))
            .when_some(error_text, |this, err| {
                this.child(
                    Button::new(SharedString::from(format!("copy-error-{id}")))
                        .label(copy_error_label)
                        .ghost()
                        .compact()
                        .tooltip(copy_error_tip)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(err.to_string()));
                            this.status = copied_status.clone();
                            cx.notify();
                        })),
                )
            })
            .when(pending, |this| {
                this.child(
                    Button::new(SharedString::from(format!("cancel-retry-{id}")))
                        .label(cancel_retry_label)
                        .secondary()
                        .outline()
                        .compact()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.cancel_auto_reconnect(id, cx);
                        })),
                )
            })
            .when(reconnectable, |this| {
                this.child(
                    Button::new(SharedString::from(format!("reconnect-{id}")))
                        .label(reconnect_label)
                        .primary()
                        .compact()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.reconnect_tab(id, window, cx);
                        })),
                )
            })
            .child(
                div()
                    .id(SharedString::from(format!("ended-close-{id}")))
                    .px_1()
                    .text_base()
                    .cursor_pointer()
                    .text_color(fg)
                    .hover(|this| this.text_color(pal.danger))
                    .child("×")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_tab(id, cx);
                    })),
            )
            .into_any_element();
        v_flex()
            .id(SharedString::from(format!("ended-banner-{id}")))
            .absolute()
            .top(px(8.0))
            .left(px(8.0))
            .right(px(8.0))
            .px_3()
            .py_2()
            .gap_1()
            .rounded(px(6.0))
            .bg(theme.warning)
            .text_color(fg)
            .shadow_md()
            .child(head_row)
            .when_some(error_line, |this, line| {
                this.child(div().text_xs().child(line))
            })
            .into_any_element()
    }


}

/// 把多行错误压成一行并按字符数截断：横幅宽度有限，完整原文交给「复制错误」。
fn flatten_error_text(text: &str, max_chars: usize) -> SharedString {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(max_chars).collect();
    if flat.chars().count() > max_chars {
        out.push('…');
    }
    SharedString::from(out)
}
