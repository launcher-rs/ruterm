//! 远程桌面标签：标签内就地连接表单、VNC 帧渲染、输入转发与工具条。
//!
//! 约定与终端一致：网络 IO 全在 tokio 侧（`crate::desktop`），这里只收发事件、
//! 把整帧 BGRA 快照换成 `RenderImage` 贴到画布上。密码只留在本次会话的内存里，
//! 不落盘、不进日志、不进命令行。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rgpui::input_ui::{Input, InputContentType, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Bounds, Button, ButtonVariants, ClipboardEntry, ClipboardItem, Context, Corners,
    Entity, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, KeyUpEvent, Keystroke,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled,
    Tooltip, Window, canvas, div, h_flex, point, px, v_flex,
};

use crate::desktop::{Command, Event, Session, Snapshot};
use crate::tr;

use super::*;

/// 远程桌面会话阶段：表单 → 连接中 → 已连接；失败保留标签以便原地重试。
pub(super) enum DesktopPhase {
    Form,
    Connecting,
    Live,
    Failed { message: SharedString },
}

/// 远程桌面标签的全部状态。
pub(super) struct DesktopTab {
    /// 最近一次连接的地址参数（重试与工具条展示用）。
    pub(super) host: String,
    pub(super) port: u16,
    /// VNC 密码：只存活在内存，标签关闭即丢弃；绝不写库 / 写日志。
    pub(super) password: String,
    /// 共享桌面（VNC shared 标志）。
    pub(super) shared: bool,
    pub(super) phase: DesktopPhase,
    /// 标签内连接表单的三个输入框。
    pub(super) form_host: Entity<InputState>,
    pub(super) form_port: Entity<InputState>,
    pub(super) form_password: Entity<InputState>,
    /// 画面焦点：点击画面后键盘事件走这个句柄。
    pub(super) focus: FocusHandle,
    /// 当前帧（BGRA）。换帧时旧图进 `drop_queue`，在下一次 render 里交给窗口释放。
    pub(super) image: Option<Arc<RenderImage>>,
    pub(super) drop_queue: Vec<Arc<RenderImage>>,
    /// 画面实际绘制区域（窗口坐标），由画布 prepaint 回填，指针坐标据此反算远端像素。
    pub(super) draw_rect: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 远端桌面尺寸（像素）。
    pub(super) size: (u16, u16),
    /// 服务器名（ServerInit 里的桌面名），工具条悬停提示用。
    pub(super) name: String,
    /// 0.0＝自适应窗口；其余为缩放倍数（1.0＝100%）。
    pub(super) zoom: f32,
    /// 捕获键盘：开启后按键转发到远端，Esc 释放。
    pub(super) capture_keys: bool,
    /// 当前按下的鼠标键位图（bit0 左 / bit1 中 / bit2 右）。
    pub(super) buttons: u8,
    /// 会话指令通道（断开 / 输入 / 剪贴板 / 请求整帧）。
    pub(super) command: Option<tokio::sync::mpsc::UnboundedSender<Command>>,
    /// 最新帧槽位：事件循环取走后协议层才产下一帧。
    pub(super) latest: Option<Arc<Mutex<Option<Snapshot>>>>,
    /// 实际帧率（每秒由协议层汇报）。
    pub(super) fps: f32,
    /// 连接代号：重连后旧事件循环的迟到事件按代号丢弃。
    pub(super) token: u64,
}

impl DesktopTab {
    pub(super) fn send(&self, command: Command) {
        if let Some(tx) = self.command.as_ref() {
            let _ = tx.send(command);
        }
    }

    fn clear_buttons(&mut self) {
        self.buttons = 0;
    }
}

impl RuTermApp {
    /// 「会话 → 远程桌面…」：新建一个空的桌面标签，先显示就地表单。
    pub(super) fn new_desktop(
        &mut self,
        _: &crate::NewRemoteDesktop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        self.view_split = None;
        let form_host = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!(cx, "主机或 IP（可写 host:port）"))
        });
        let form_port = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("5900")
                .placeholder("5900")
        });
        let form_password = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!(cx, "VNC 密码（可留空）"))
        });
        // 表单里按回车直接开连，省一次鼠标。
        for input in [&form_host, &form_port, &form_password] {
            let connect_id = id;
            cx.subscribe(input, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.begin_desktop_connect(connect_id, cx);
                }
            })
            .detach();
        }
        let desktop = DesktopTab {
            host: String::new(),
            port: 5900,
            password: String::new(),
            shared: true,
            phase: DesktopPhase::Form,
            form_host,
            form_port,
            form_password,
            focus: cx.focus_handle(),
            image: None,
            drop_queue: Vec::new(),
            draw_rect: Rc::new(Cell::new(None)),
            size: (0, 0),
            name: String::new(),
            zoom: 0.0,
            capture_keys: false,
            buttons: 0,
            command: None,
            latest: None,
            fps: 0.0,
            token: 0,
        };
        self.tabs.push(SessionTab {
            id,
            title: tr!(cx, "远程桌面"),
            base_title: tr!(cx, "远程桌面"),
            custom_title: None,
            addr: SharedString::default(),
            auth: None,
            remote_info: None,
            terminal_view: None,
            desktop: Some(desktop),
            ended: false,
            failed: false,
            last_error: None,
            resume: None,
            disconnect: None,
            commands: None,
            forwards: Vec::new(),
            files: FileBrowser::default(),
            terminal_cwd: None,
            zmodem: None,
            zmodem_progress: None,
            auto_retry_attempt: 0,
            auto_retry_pending: false,
            connected_at: None,
        });
        self.active_tab = Some(id);
        self.activate_tab(id, window, cx);
        self.status = tr!(cx, "填写地址后按回车或点「连接」");
        self.sync_tray(cx);
        cx.notify();
    }

    /// 表单「连接」：读参数 → 进入连接中 → 后台建连。
    pub(super) fn begin_desktop_connect(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        let Some(desktop) = tab.desktop.as_ref() else {
            return;
        };
        let raw = desktop.form_host.read(cx).value().trim().to_string();
        let port_text = desktop.form_port.read(cx).value().trim().to_string();
        let password = desktop.form_password.read(cx).value().to_string();
        let (host, port) = normalize_address(&raw, &port_text);
        if host.is_empty() {
            self.status = tr!(cx, "远程桌面：请填写主机地址").into();
            cx.notify();
            return;
        }
        let Some(port) = port else {
            self.status = tr!(cx, "远程桌面：端口无效").into();
            cx.notify();
            return;
        };
        self.open_desktop_session(id, host, port, password, desktop.shared, cx);
    }

    /// 连接中「取消」：令牌加一让在途的建连结果作废，回到就地表单。
    pub(super) fn cancel_desktop_connect(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        {
            desktop.token += 1;
            desktop.phase = DesktopPhase::Form;
            desktop.command = None;
            desktop.latest = None;
            desktop.clear_buttons();
        }
        if self.active_tab == Some(id) {
            self.status = tr!(cx, "已取消连接").into();
        }
        cx.notify();
    }

    /// 失败横幅的「重试」：沿用上次连接参数（密码仍在内存里）。
    pub(super) fn retry_desktop(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
        else {
            return;
        };
        if desktop.host.is_empty() {
            self.set_desktop_form(id, cx);
            return;
        }
        let (host, port, password, shared) = (
            desktop.host.clone(),
            desktop.port,
            desktop.password.clone(),
            desktop.shared,
        );
        self.open_desktop_session(id, host, port, password, shared, cx);
    }

    /// 失败横幅的「改参数」：回到就地表单。
    pub(super) fn set_desktop_form(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        {
            desktop.phase = DesktopPhase::Form;
            cx.notify();
        }
    }

    fn open_desktop_session(
        &mut self,
        id: usize,
        host: String,
        port: u16,
        password: String,
        shared: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let Some(desktop) = tab.desktop.as_mut() else {
            return;
        };
        desktop.token += 1;
        let token = desktop.token;
        desktop.host = host.clone();
        desktop.port = port;
        desktop.password = password.clone();
        desktop.shared = shared;
        desktop.phase = DesktopPhase::Connecting;
        desktop.command = None;
        desktop.latest = None;
        desktop.fps = 0.0;
        desktop.clear_buttons();
        if let Some(old) = desktop.image.take() {
            desktop.drop_queue.push(old);
        }
        tab.ended = false;
        tab.failed = false;
        tab.last_error = None;
        let addr = host_port(&host, port);
        tab.addr = addr.clone().into();
        if tab.custom_title.is_none() {
            let title = SharedString::from(addr.clone());
            tab.title = title.clone();
            tab.base_title = title;
        }
        let active = self.active_tab == Some(id);
        if active {
            self.status = tr!(cx, "正在连接远程桌面 {addr}…", addr = addr);
        }
        let config = crate::desktop::Config {
            host,
            port,
            password: (!password.is_empty()).then_some(password),
            shared,
        };
        let handle = rgpui::tokio::Tokio::handle(cx);
        cx.spawn(async move |this, cx| {
            let result = handle
                .spawn(async move { crate::desktop::connect(config).await })
                .await;
            let outcome = match result {
                Ok(result) => result,
                Err(error) => Err(anyhow::Error::new(error)),
            };
            let _ = this.update(cx, |this, cx| match outcome {
                Ok(session) => this.attach_desktop_session(id, token, session, cx),
                Err(error) => this.fail_desktop(id, token, format!("{error:#}"), cx),
            });
        })
        .detach();
        self.sync_tray(cx);
        cx.notify();
    }

    fn attach_desktop_session(
        &mut self,
        id: usize,
        token: u64,
        session: Session,
        cx: &mut Context<Self>,
    ) {
        let Session {
            command,
            latest,
            events,
        } = session;
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            let _ = command.send(Command::Close);
            return;
        };
        let Some(desktop) = tab.desktop.as_mut() else {
            return;
        };
        if desktop.token != token {
            let _ = command.send(Command::Close);
            return;
        }
        desktop.command = Some(command);
        desktop.latest = Some(latest.clone());
        desktop.phase = DesktopPhase::Live;
        desktop.capture_keys = true;
        tab.ended = false;
        tab.failed = false;
        tab.last_error = None;
        tab.connected_at = Some(std::time::Instant::now());
        if tab.custom_title.is_none() {
            let title = SharedString::from(host_port(&desktop.host, desktop.port));
            tab.title = title.clone();
            tab.base_title = title;
        }
        self.status = tr!(cx, "已连接远程桌面");
        self.sync_tray(cx);
        cx.notify();

        // 事件循环：只在这里搬运事件；帧快照取走后协议层才会产下一帧。
        let mut events = events;
        let latest_slot = latest;
        cx.spawn(async move |this, cx| {
            loop {
                let Some(event) = events.recv().await else {
                    break;
                };
                let alive = match event {
                    Event::FrameReady => {
                        let snapshot = latest_slot
                            .lock()
                            .ok()
                            .and_then(|mut slot| slot.take());
                        match snapshot {
                            Some(snapshot) => {
                                let image = snapshot_to_image(&snapshot);
                                let size = (snapshot.width, snapshot.height);
                                this.update(cx, |this, cx| {
                                    this.apply_desktop_frame(id, token, image, size, cx);
                                })
                                .is_ok()
                            }
                            None => true,
                        }
                    }
                    Event::Connected {
                        width,
                        height,
                        name,
                    } => this
                        .update(cx, |this, cx| {
                            this.apply_desktop_connected(id, token, width, height, name, cx);
                        })
                        .is_ok(),
                    Event::Fps { fps } => this
                        .update(cx, |this, cx| this.apply_desktop_fps(id, token, fps, cx))
                        .is_ok(),
                    Event::Clipboard(text) => this
                        .update(cx, |this, cx| this.apply_desktop_clipboard(id, token, text, cx))
                        .is_ok(),
                    Event::Closed { reason } => {
                        let _ = this.update(cx, |this, cx| {
                            this.finish_desktop(id, token, reason, cx);
                        });
                        false
                    }
                };
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_desktop_frame(
        &mut self,
        id: usize,
        token: u64,
        image: Arc<RenderImage>,
        size: (u16, u16),
        cx: &mut Context<Self>,
    ) {
        let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        else {
            return;
        };
        if desktop.token != token {
            return;
        }
        if let Some(old) = desktop.image.replace(image) {
            desktop.drop_queue.push(old);
        }
        desktop.size = size;
        cx.notify();
    }

    /// 握手完成：拿到服务器名与桌面尺寸（首帧到达前工具条就有内容）。
    fn apply_desktop_connected(
        &mut self,
        id: usize,
        token: u64,
        width: u16,
        height: u16,
        name: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
            && desktop.token == token
        {
            desktop.name = name;
            desktop.size = (width, height);
            cx.notify();
        }
    }

    fn apply_desktop_fps(&mut self, id: usize, token: u64, fps: f32, cx: &mut Context<Self>) {        if let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
            && desktop.token == token
        {
            desktop.fps = fps;
            cx.notify();
        }
    }

    fn apply_desktop_clipboard(
        &mut self,
        id: usize,
        token: u64,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
            .is_some_and(|desktop| desktop.token == token);
        if !current {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        if self.active_tab == Some(id) {
            self.status = tr!(cx, "已收到远端剪贴板").into();
        }
        cx.notify();
    }

    fn fail_desktop(&mut self, id: usize, token: u64, message: String, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let Some(desktop) = tab.desktop.as_mut() else {
            return;
        };
        if desktop.token != token {
            return;
        }
        let message = SharedString::from(message);
        desktop.phase = DesktopPhase::Failed {
            message: message.clone(),
        };
        desktop.command = None;
        desktop.latest = None;
        desktop.capture_keys = false;
        desktop.clear_buttons();
        tab.ended = true;
        tab.failed = true;
        tab.last_error = Some(message.clone());
        if self.active_tab == Some(id) {
            self.status = message;
        }
        self.sync_tray(cx);
        cx.notify();
    }

    /// 会话结束（远端关闭或主动断开）：保留标签，失败卡片给重试入口。
    fn finish_desktop(&mut self, id: usize, token: u64, reason: String, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let Some(desktop) = tab.desktop.as_mut() else {
            return;
        };
        if desktop.token != token {
            return;
        }
        let reason = SharedString::from(reason);
        desktop.phase = DesktopPhase::Failed {
            message: reason.clone(),
        };
        desktop.command = None;
        desktop.latest = None;
        desktop.capture_keys = false;
        desktop.clear_buttons();
        tab.ended = true;
        tab.failed = false;
        tab.last_error = Some(reason.clone());
        if self.active_tab == Some(id) {
            self.status = reason;
        }
        self.sync_tray(cx);
        cx.notify();
    }

    fn close_desktop(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        else {
            return;
        };
        desktop.send(Command::Close);
        desktop.capture_keys = false;
        desktop.clear_buttons();
        if self.active_tab == Some(id) {
            self.status = tr!(cx, "正在断开远程桌面…").into();
        }
        cx.notify();
    }

    fn send_desktop_pointer(&mut self, id: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        else {
            return;
        };
        let Some(rect) = desktop.draw_rect.get() else {
            return;
        };
        match remote_point(rect, desktop.size, position) {
            Some((x, y)) => desktop.send(Command::Pointer {
                x,
                y,
                buttons: desktop.buttons,
            }),
            // 拖到画面外的留白：就地松键，免得远端一直按着某个键。
            None if desktop.buttons != 0 => {
                desktop.buttons = 0;
                let (x, y) = remote_point_clamped(rect, desktop.size, position);
                desktop.send(Command::Pointer { x, y, buttons: 0 });
            }
            None => {}
        }
        let _ = cx;
    }

    fn set_desktop_button(&mut self, id: usize, button: MouseButton, down: bool, cx: &mut Context<Self>) {
        let bit = match button {
            MouseButton::Left => 0b001,
            MouseButton::Middle => 0b010,
            MouseButton::Right => 0b100,
            _ => return,
        };
        let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        else {
            return;
        };
        if down {
            desktop.buttons |= bit;
        } else {
            desktop.buttons &= !bit;
        }
        cx.notify();
    }

    fn send_desktop_wheel(&mut self, id: usize, up: bool, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        else {
            return;
        };
        let Some(rect) = desktop.draw_rect.get() else {
            return;
        };
        let Some((x, y)) = remote_point(rect, desktop.size, position) else {
            return;
        };
        // VNC 没有滚轮事件：上/下滚就是滚轮按钮（4/5）的按下与释放。
        let button = if up { 4 } else { 5 };
        let base = desktop.buttons;
        desktop.send(Command::Pointer {
            x,
            y,
            buttons: base | button,
        });
        desktop.send(Command::Pointer {
            x,
            y,
            buttons: base,
        });
        let _ = cx;
    }

    fn desktop_key(&mut self, id: usize, event: &Keystroke, down: bool, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        else {
            return;
        };
        if !desktop.capture_keys {
            return;
        }
        if let Some(keysym) = crate::desktop::keysym::event_keysym(
            &event.key,
            event.key_char.as_deref(),
        ) {
            desktop.send(Command::Key { keysym, down });
        }
        let _ = cx;
    }

    /// 键盘按下：捕获开启时全部转给远端，Esc 先用来释放捕获（不发给远端）。
    pub(super) fn desktop_key_down(
        &mut self,
        id: usize,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(desktop) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
        else {
            return;
        };
        if !desktop.capture_keys || !desktop.focus.is_focused(window) {
            return;
        }
        cx.stop_propagation();
        if event.keystroke.key.eq_ignore_ascii_case("escape") {
            if let Some(desktop) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == id)
                .and_then(|tab| tab.desktop.as_mut())
            {
                desktop.capture_keys = false;
            }
            self.status = tr!(cx, "已释放键盘，点击画面可重新捕获").into();
            cx.notify();
            return;
        }
        self.desktop_key(id, &event.keystroke, true, cx);
    }

    pub(super) fn desktop_key_up(
        &mut self,
        id: usize,
        event: &KeyUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(desktop) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
        else {
            return;
        };
        if !desktop.capture_keys || !desktop.focus.is_focused(window) {
            return;
        }
        cx.stop_propagation();
        self.desktop_key(id, &event.keystroke, false, cx);
    }

    /// 工具条：把本地剪贴板文本发给远端。
    fn send_clipboard_to_desktop(&mut self, id: usize, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|item| {
            item.entries.into_iter().find_map(|entry| match entry {
                ClipboardEntry::String(text) => Some(text.text),
                _ => None,
            })
        });
        let Some(text) = text else {
            self.status = tr!(cx, "剪贴板里没有文本").into();
            cx.notify();
            return;
        };
        let Some(desktop) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
        else {
            return;
        };
        desktop.send(Command::Clipboard(text));
        self.status = tr!(cx, "已把剪贴板发送到远端").into();
        cx.notify();
    }

    fn request_desktop_full(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(desktop) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
        {
            desktop.send(Command::RequestFull);
        }
        let _ = cx;
    }

    fn set_desktop_zoom(&mut self, id: usize, zoom: f32, cx: &mut Context<Self>) {
        if let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        {
            desktop.zoom = zoom;
            cx.notify();
        }
    }

    fn toggle_desktop_capture(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        else {
            return;
        };
        desktop.capture_keys = !desktop.capture_keys;
        let on = desktop.capture_keys;
        if on {
            desktop.focus.focus(window, cx);
        }
        self.status = if on {
            tr!(cx, "键盘已捕获（Esc 释放）").into()
        } else {
            tr!(cx, "已释放键盘").into()
        };
        cx.notify();
    }

    /// 远程桌面标签的主体：表单 / 连接中 / 失败横幅 / 画面 + 工具条。
    pub(super) fn render_desktop_pane(
        &self,
        tab: &SessionTab,
        desktop: &DesktopTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &desktop.phase {
            DesktopPhase::Form => self.render_desktop_form(tab.id, desktop, cx),
            DesktopPhase::Connecting => {
                let pal = self.palette(cx);
                let id = tab.id;
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(div().text_sm().child(tr!(cx, "正在连接远程桌面…")))
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(host_port(&desktop.host, desktop.port)),
                    )
                    .child(
                        Button::new(SharedString::from(format!("rd-cancel-{id}")))
                            .label(tr!(cx, "取消"))
                            .secondary()
                            .outline()
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cancel_desktop_connect(id, cx);
                            })),
                    )
                    .into_any_element()
            }
            DesktopPhase::Failed { message } => {
                self.render_desktop_failed(tab, desktop, message, cx)
            }
            DesktopPhase::Live => {
                let pal = self.palette(cx);
                let id = tab.id;
                v_flex()
                    .size_full()
                    .bg(pal.window_bg)
                    .child(self.render_desktop_toolbar(tab, desktop, cx))
                    .child(self.render_desktop_screen(id, desktop, cx))
                    .into_any_element()
            }
        }
    }

    fn render_desktop_screen(
        &self,
        id: usize,
        desktop: &DesktopTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let capture = desktop.capture_keys;
        let image = desktop.image.clone();
        let draw_sink = desktop.draw_rect.clone();
        let remote = desktop.size;
        let zoom = desktop.zoom;
        let focus = desktop.focus.clone();
        let screen = div()
            .flex_1()
            .min_h_0()
            .relative()
            .overflow_hidden()
            .bg(pal.window_bg)
            .track_focus(&focus)
            .key_context("RemoteDesktop")
            .cursor_pointer()
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                this.send_desktop_pointer(id, event.position, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if let Some(desktop) = this
                        .tabs
                        .iter_mut()
                        .find(|tab| tab.id == id)
                        .and_then(|tab| tab.desktop.as_mut())
                    {
                        desktop.capture_keys = true;
                        desktop.focus.focus(window, cx);
                    }
                    this.set_desktop_button(id, MouseButton::Left, true, cx);
                    this.send_desktop_pointer(id, event.position, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    this.set_desktop_button(id, MouseButton::Left, false, cx);
                    this.send_desktop_pointer(id, event.position, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.set_desktop_button(id, MouseButton::Middle, true, cx);
                    this.send_desktop_pointer(id, event.position, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    this.set_desktop_button(id, MouseButton::Middle, false, cx);
                    this.send_desktop_pointer(id, event.position, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.set_desktop_button(id, MouseButton::Right, true, cx);
                    this.send_desktop_pointer(id, event.position, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    this.set_desktop_button(id, MouseButton::Right, false, cx);
                    this.send_desktop_pointer(id, event.position, cx);
                }),
            )
            .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                let dy = match event.delta {
                    ScrollDelta::Lines(point) => point.y,
                    ScrollDelta::Pixels(point) => point.y.as_f32(),
                };
                if dy != 0.0 {
                    this.send_desktop_wheel(id, dy > 0.0, event.position, cx);
                }
            }))
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                this.desktop_key_down(id, event, window, cx);
            }))
            .capture_key_up(cx.listener(move |this, event: &KeyUpEvent, window, cx| {
                this.desktop_key_up(id, event, window, cx);
            }))
            .child(
                canvas(
                    move |bounds, _window, _cx| {
                        let rect = desktop_draw_rect(bounds, remote, zoom);
                        draw_sink.set(Some(rect));
                        rect
                    },
                    move |_bounds, rect, window, _cx| {
                        if let Some(image) = image.as_ref() {
                            let _ = window.paint_image(
                                rect,
                                Corners::default(),
                                image.clone(),
                                0,
                                false,
                            );
                        }
                    },
                )
                .size_full(),
            )
            // 键盘没捕获时给一条轻提示：否则用户会以为按键没反应。
            .when(!capture, |this| {
                let hint = tr!(cx, "点击画面捕获键盘，Esc 释放");
                this.child(
                    div()
                        .absolute()
                        .top_2()
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded(px(4.0))
                                .bg(pal.card_bg)
                                .border_1()
                                .border_color(pal.border)
                                .text_xs()
                                .text_color(pal.muted)
                                .child(hint),
                        ),
                )
            });
        screen.into_any_element()
    }

    fn render_desktop_toolbar(
        &self,
        tab: &SessionTab,
        desktop: &DesktopTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let id = tab.id;
        let size_text = if desktop.size.0 > 0 {
            format!(
                "{}×{} · {:.0} fps",
                desktop.size.0, desktop.size.1, desktop.fps
            )
        } else {
            "—".to_string()
        };
        let name_tip = if desktop.name.is_empty() {
            tr!(cx, "远端桌面").to_string()
        } else {
            desktop.name.clone()
        };
        h_flex()
            .flex_shrink_0()
            .h(px(38.0))
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(pal.border)
            .bg(pal.card_bg)
            .child(
                Button::new(SharedString::from(format!("rd-disconnect-{id}")))
                    .label(tr!(cx, "断开"))
                    .ghost()
                    .compact()
                    .on_click(cx.listener(move |this, _, _, cx| this.close_desktop(id, cx))),
            )
            .child(
                Button::new(SharedString::from(format!("rd-fit-{id}")))
                    .label(tr!(cx, "自适应"))
                    .ghost()
                    .compact()
                    .selected(desktop.zoom <= 0.0)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_desktop_zoom(id, 0.0, cx);
                        this.request_desktop_full(id, cx);
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("rd-zoom-out-{id}")))
                    .label("−")
                    .ghost()
                    .compact()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let zoom = this
                            .tabs
                            .iter()
                            .find(|tab| tab.id == id)
                            .and_then(|tab| tab.desktop.as_ref())
                            .map(|desktop| desktop.zoom)
                            .unwrap_or(0.0);
                        let next = if zoom <= 0.0 {
                            1.0
                        } else {
                            (zoom - 0.25).max(0.25)
                        };
                        this.set_desktop_zoom(id, next, cx);
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("rd-zoom-reset-{id}")))
                    .label("100%")
                    .ghost()
                    .compact()
                    .selected((desktop.zoom - 1.0).abs() < f32::EPSILON)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_desktop_zoom(id, 1.0, cx);
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("rd-zoom-in-{id}")))
                    .label("+")
                    .ghost()
                    .compact()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let zoom = this
                            .tabs
                            .iter()
                            .find(|tab| tab.id == id)
                            .and_then(|tab| tab.desktop.as_ref())
                            .map(|desktop| desktop.zoom)
                            .unwrap_or(0.0);
                        let next = if zoom <= 0.0 {
                            1.0
                        } else {
                            (zoom + 0.25).min(4.0)
                        };
                        this.set_desktop_zoom(id, next, cx);
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("rd-capture-{id}")))
                    .label(tr!(cx, "捕获键盘"))
                    .ghost()
                    .compact()
                    .selected(desktop.capture_keys)
                    .tooltip(tr!(cx, "捕获后按键直接送给远端，按 Esc 释放"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_desktop_capture(id, window, cx);
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("rd-clipboard-{id}")))
                    .label(tr!(cx, "剪贴板"))
                    .ghost()
                    .compact()
                    .tooltip(tr!(cx, "把本地剪贴板文本发送到远端"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.send_clipboard_to_desktop(id, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id(SharedString::from(format!("rd-size-{id}")))
                    .text_xs()
                    .text_color(pal.muted)
                    .tooltip(move |window, cx| Tooltip::new(name_tip.clone()).build(window, cx))
                    .child(size_text),
            )
            .into_any_element()
    }

    fn render_desktop_form(&self, tab_id: usize, desktop: &DesktopTab, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w(px(380.0))
                    .gap_3()
                    .p_4()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .child(
                        div()
                            .text_base()
                            .font_weight(rgpui::FontWeight(600.0))
                            .child(tr!(cx, "远程桌面（VNC）")),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(pal.muted).child(tr!(cx, "地址")))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .child(styled_input(Input::new(&desktop.form_host), &pal)),
                                    )
                                    .child(
                                        div()
                                            .w(px(96.0))
                                            .child(styled_input(Input::new(&desktop.form_port), &pal)),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(pal.muted).child(tr!(cx, "密码")))
                            .child(styled_input(
                                Input::new(&desktop.form_password)
                                    .content_type(InputContentType::Password)
                                    .mask_toggle(),
                                &pal,
                            )),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(pal.muted)
                                    .child(tr!(cx, "密码只在本次连接的内存里使用，不会保存")),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new(SharedString::from(format!("rd-connect-{tab_id}")))
                                    .label(tr!(cx, "连接"))
                                    .primary()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.begin_desktop_connect(tab_id, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_desktop_failed(
        &self,
        tab: &SessionTab,
        desktop: &DesktopTab,
        message: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let id = tab.id;
        let message = message.clone();
        let copy_message = message.clone();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w(px(440.0))
                    .gap_3()
                    .p_4()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.danger)
                    .child(
                        div()
                            .text_base()
                            .font_weight(rgpui::FontWeight(600.0))
                            .child(tr!(cx, "远程桌面已断开")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(host_port(&desktop.host, desktop.port)),
                    )
                    .child(div().text_xs().child(message))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new(SharedString::from(format!("rd-copy-error-{id}")))
                                    .label(tr!(cx, "复制错误"))
                                    .ghost()
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_message.to_string(),
                                        ));
                                        this.status = tr!(cx, "错误已复制到剪贴板").into();
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("rd-edit-form-{id}")))
                                    .label(tr!(cx, "修改参数"))
                                    .secondary()
                                    .outline()
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_desktop_form(id, cx);
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("rd-retry-{id}")))
                                    .label(tr!(cx, "重试"))
                                    .primary()
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_desktop(id, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// render 阶段把换下来的旧帧交给窗口释放（精灵图集按图像 id 缓存，不释放会越积越多）。
    pub(super) fn drop_desktop_images(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for tab in self.tabs.iter_mut() {
            let Some(desktop) = tab.desktop.as_mut() else {
                continue;
            };
            for image in desktop.drop_queue.drain(..) {
                cx.drop_image(image, Some(window));
            }
        }
    }
}

/// 状态栏 / 标题用的 `host:port`；IPv6 字面量补方括号。
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// `host` 字段里带端口时以它为准（支持 `host:5900` 与 `[::1]:5900`）。
fn normalize_address(raw: &str, port_text: &str) -> (String, Option<u16>) {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once("]:") {
            if let Ok(port) = tail.trim().parse::<u16>() {
                // 连接用裸 IPv6 字面量，展示时再由 `host_port` 补回方括号。
                return (host.to_string(), Some(port));
            }
        }
    }
    if let Some((host, tail)) = raw.rsplit_once(':')
        && !host.contains(':')
        && !host.is_empty()
        && let Ok(port) = tail.trim().parse::<u16>()
    {
        return (host.to_string(), Some(port));
    }
    let port = port_text
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0);
    (raw.to_string(), port)
}

/// 画面绘制区域：自适应＝等比放进可用区域并居中；缩放＝固定倍数。
fn desktop_draw_rect(bounds: Bounds<Pixels>, remote: (u16, u16), zoom: f32) -> Bounds<Pixels> {
    let width = f32::from(remote.0).max(1.0);
    let height = f32::from(remote.1).max(1.0);
    let available_w = bounds.size.width.as_f32().max(1.0);
    let available_h = bounds.size.height.as_f32().max(1.0);
    let scale = if zoom <= 0.0 {
        (available_w / width).min(available_h / height)
    } else {
        zoom
    };
    let draw_w = width * scale;
    let draw_h = height * scale;
    Bounds {
        origin: point(
            bounds.origin.x + px((available_w - draw_w) / 2.0),
            bounds.origin.y + px((available_h - draw_h) / 2.0),
        ),
        size: rgpui::size(px(draw_w), px(draw_h)),
    }
}

/// 窗口坐标 → 远端像素坐标；落在画面外（上下/左右留白）时不发送。
fn remote_point(
    rect: Bounds<Pixels>,
    remote: (u16, u16),
    position: Point<Pixels>,
) -> Option<(u16, u16)> {
    if remote.0 == 0 || remote.1 == 0 {
        return None;
    }
    let width = rect.size.width.as_f32();
    let height = rect.size.height.as_f32();
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let x = (position.x - rect.origin.x).as_f32() / width;
    let y = (position.y - rect.origin.y).as_f32() / height;
    if !(0.0..1.0).contains(&x) || !(0.0..1.0).contains(&y) {
        return None;
    }
    let px_x = (x * f32::from(remote.0)) as u16;
    let px_y = (y * f32::from(remote.1)) as u16;
    Some((px_x.min(remote.0 - 1), px_y.min(remote.1 - 1)))
}

/// 窗口坐标 → 远端像素坐标，落到画面外时夹到最近的边缘（松键用）。
fn remote_point_clamped(
    rect: Bounds<Pixels>,
    remote: (u16, u16),
    position: Point<Pixels>,
) -> (u16, u16) {
    if remote.0 == 0 || remote.1 == 0 {
        return (0, 0);
    }
    let width = rect.size.width.as_f32().max(1.0);
    let height = rect.size.height.as_f32().max(1.0);
    let x = ((position.x - rect.origin.x).as_f32() / width).clamp(0.0, 1.0);
    let y = ((position.y - rect.origin.y).as_f32() / height).clamp(0.0, 1.0);
    let px_x = (x * f32::from(remote.0)) as u16;
    let px_y = (y * f32::from(remote.1)) as u16;
    (px_x.min(remote.0 - 1), px_y.min(remote.1 - 1))
}

/// BGRA 快照 → rgpui 纹理帧（`RenderImage` 的数据顺序就是 BGRA）。
fn snapshot_to_image(snapshot: &Snapshot) -> Arc<RenderImage> {
    let width = u32::from(snapshot.width);
    let height = u32::from(snapshot.height);
    let buffer = image::RgbaImage::from_raw(width, height, snapshot.bgra.clone())
        .unwrap_or_else(|| image::RgbaImage::new(1, 1));
    Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_parsing_prefers_explicit_port() {
        assert_eq!(
            normalize_address("192.168.1.2:5901", "5900"),
            ("192.168.1.2".to_string(), Some(5901))
        );
        assert_eq!(
            normalize_address("example.com", "5900"),
            ("example.com".to_string(), Some(5900))
        );
        assert_eq!(
            normalize_address("[::1]:5902", "5900"),
            ("::1".to_string(), Some(5902))
        );
        assert_eq!(
            normalize_address("example.com", "nope"),
            ("example.com".to_string(), None)
        );
        // 展示时 IPv6 字面量补回方括号，IPv4 / 域名保持原样。
        assert_eq!(host_port("::1", 5902), "[::1]:5902");
        assert_eq!(host_port("example.com", 5900), "example.com:5900");
    }

    #[test]
    fn remote_point_maps_inside_and_rejects_letterbox() {
        let rect = Bounds {
            origin: point(px(10.0), px(20.0)),
            size: rgpui::size(px(200.0), px(100.0)),
        };
        assert_eq!(
            remote_point(rect, (800, 400), point(px(110.0), px(70.0))),
            Some((400, 200))
        );
        // 落在画面左侧留白处：不发送。
        assert_eq!(
            remote_point(rect, (800, 400), point(px(5.0), px(70.0))),
            None
        );
    }
}
