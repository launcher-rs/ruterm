//! 远程桌面标签：标签内就地连接表单、VNC 帧渲染、输入转发与工具条。
//!
//! 约定与终端一致：网络 IO 全在 tokio 侧（`crate::desktop`），这里只收发事件、
//! 把整帧 BGRA 快照换成 `RenderImage` 贴到画布上。密码在加密库可用时随主机加密入库
//! （与 SSH 密码同一套，随主库锁定），否则只留在内存；不进日志、不进命令行。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rgpui::components::Select;
use rgpui::input_ui::{Input, InputContentType, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Bounds, Button, ButtonVariants, Checkbox, ClipboardEntry, ClipboardItem, Context,
    Corners, Entity, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, KeyUpEvent,
    Keystroke, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, RenderImage, ScrollDelta, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Tooltip, Window, canvas, div,
    h_flex, point, px, v_flex,
};

use crate::desktop::{Command, Event, KeyInput, Protocol, Session, Snapshot};
use crate::sessions::{AuthMethod, HostConfig, ProtocolKind};
use crate::tr;
use crate::vault::Vault;

use super::*;

/// 远程桌面会话阶段：表单 → 连接中 → 已连接；失败保留标签以便原地重试。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DesktopPhase {
    Form,
    /// 已保存主机直连时缺密码：只问密码，不展示完整表单。
    NeedPassword,
    Connecting,
    Live,
    Failed {
        message: SharedString,
    },
}

/// 换下来的旧帧最多在 `drop_queue` 里压几张：不渲染（最小化 / 被完全遮挡）
/// 时队列只进不出，1080p 一张约 8MB，不设上限会随帧数积到 GB 级。
/// 超限的最老帧就地释放（见 `apply_desktop_frame`）。
const DESKTOP_DROP_QUEUE_CAP: usize = 4;

/// 打开桌面表单时预填的参数（来自已保存主机或局域网发现）。
pub(super) struct DesktopPrefill {
    pub(super) protocol: Protocol,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) username: String,
    pub(super) domain: String,
    /// 来源已保存主机 id：保存时原位更新，避免重复建条目。
    pub(super) saved_host: Option<i64>,
    /// 打开即连接（已保存主机），不再停在完整表单。
    pub(super) auto_connect: bool,
}

/// 远程桌面标签的全部状态。
pub(super) struct DesktopTab {
    /// 最近一次连接的地址参数（重试与工具条展示用）。
    pub(super) host: String,
    pub(super) port: u16,
    /// 表单里选的协议（VNC / RDP）。
    pub(super) protocol: Protocol,
    /// 来源已保存主机 id（右键「编辑」/主机行点开时回填），保存时原位更新。
    pub(super) saved_host: Option<i64>,
    /// 从已保存主机点开：直接连接，缺密码时只问密码。
    pub(super) direct: bool,
    /// 本次连接参数的缓存键（`user@host:port`），成功后密码按它留在内存里。
    pub(super) cache_key: Option<String>,
    /// 保存到哪个分组；None＝未分组。打开已保存主机时回填原归属。
    pub(super) group: Option<SharedString>,
    /// 远程桌面密码：标签关闭即从内存丢弃；保存主机或连接成功时加密写入凭据库。
    pub(super) password: String,
    /// RDP 账号与域（VNC 不用）：同样只留在内存。
    pub(super) username: String,
    pub(super) domain: String,
    /// RDP 请求的桌面尺寸（远端按这个分辨率建会话）。
    pub(super) request_size: (u16, u16),
    /// 共享桌面（VNC shared 标志）。
    pub(super) shared: bool,
    pub(super) phase: DesktopPhase,
    /// 标签内连接表单的输入框。
    pub(super) form_host: Entity<InputState>,
    pub(super) form_port: Entity<InputState>,
    pub(super) form_password: Entity<InputState>,
    pub(super) form_username: Entity<InputState>,
    pub(super) form_domain: Entity<InputState>,
    /// 表单内「保存密码」开关：与 SSH 一致，仅勾选后才把密码加密入库。
    pub(super) save_secret: bool,
    /// 表单内的校验 / 认证失败提示（错误只留在标签里，不弹系统对话框）。
    pub(super) form_error: Option<SharedString>,
    /// 画面焦点：点击画面后键盘事件走这个句柄。
    pub(super) focus: FocusHandle,
    /// 当前帧（BGRA）。换帧时旧图进 `drop_queue`，在下一次 render 里交给窗口释放。
    pub(super) image: Option<Arc<RenderImage>>,
    pub(super) drop_queue: Vec<Arc<RenderImage>>,
    /// 画面实际绘制区域（窗口坐标），由画布 prepaint 回填，指针坐标据此反算远端像素。
    pub(super) draw_rect: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 画面可用区域（整块画布，窗口坐标），「匹配窗口」据此算目标分辨率。
    pub(super) screen_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 远端桌面尺寸（像素）。
    pub(super) size: (u16, u16),
    /// 服务器名（ServerInit 里的桌面名），工具条悬停提示用。
    pub(super) name: String,
    /// 0.0＝自适应窗口；其余为缩放倍数（1.0＝100%）。
    pub(super) zoom: f32,
    /// 捕获键盘：开启后按键转发到远端，Esc 释放。
    pub(super) capture_keys: bool,
    /// 已转发到远端的修饰键状态。rgpui 对 Ctrl / Shift / Alt / Win 只发
    /// `ModifiersChangedEvent`、不发 KeyDown / KeyUp，这里按差异补发按下 / 抬起，
    /// 远端才认得 Ctrl+V 这类组合键。
    pub(super) modifiers: Modifiers,
    /// 上次看到的 CapsLock 状态：切换时补一次按下 + 抬起。
    pub(super) capslock: bool,
    /// 当前按下的鼠标键位图（bit0 左 / bit1 中 / bit2 右）。
    pub(super) buttons: u8,
    /// 会话指令通道（断开 / 输入 / 剪贴板 / 请求整帧）。
    pub(super) command: Option<tokio::sync::mpsc::UnboundedSender<Command>>,
    /// 最新帧槽位：事件循环取走后协议层才产下一帧。
    pub(super) latest: Option<Arc<Mutex<Option<Snapshot>>>>,
    /// 拖放上传进度（RDP 文件剪贴板）；没有传输时为 None。
    pub(super) upload: Option<UploadStatus>,
    /// 实际帧率（每秒由协议层汇报）。
    pub(super) fps: f32,
    /// 连接代号：重连后旧事件循环的迟到事件按代号丢弃。
    pub(super) token: u64,
    /// RDP 系统剪贴板授权令牌：同一时刻只允许活动标签的会话读写本机剪贴板。
    /// VNC 不使用（协议层自己同步文本）。
    pub(super) clipboard_token: Option<u64>,
    /// 画面焦点丢失时释放远端按键（切走窗口不能让远端一直以为键还按着）。
    /// 只为保持订阅存活而持有，不直接读取。
    pub(super) _focus_out: Option<Subscription>,
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

/// 会话结束时收回剪贴板授权：只有仍在活动的那份会被撤销，后台会话不受影响。
fn release_desktop_clipboard(desktop: &mut DesktopTab) {
    if let Some(token) = desktop.clipboard_token.take() {
        ironrdp_cliprdr_native::release_clipboard_session(token);
    }
}

/// 一次拖放上传的进度（进度条画在画面右下角）。
pub(super) struct UploadStatus {
    pub(super) done: u64,
    pub(super) total: u64,
    pub(super) files: u32,
}

impl RuTermApp {
    /// 「会话 → 远程桌面…」：新建一个空的桌面标签，先显示就地表单。
    pub(super) fn new_desktop(
        &mut self,
        _: &crate::NewRemoteDesktop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_desktop_with(None, window, cx);
    }

    /// 从局域网发现 / 主机右键打开远程桌面：地址 / 端口预填，凭据仍由用户输入。
    pub(super) fn open_desktop_prefilled(
        &mut self,
        protocol: Protocol,
        host: String,
        port: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_desktop_with(
            Some(DesktopPrefill {
                protocol,
                host,
                port,
                username: String::new(),
                domain: String::new(),
                saved_host: None,
                auto_connect: false,
            }),
            window,
            cx,
        );
    }

    /// 点开列表里已保存的远程桌面主机：直接连接；缺密码时只弹密码问一次。
    pub(super) fn open_desktop_for_saved(
        &mut self,
        host: &HostConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let protocol = match host.protocol {
            ProtocolKind::Rdp => Protocol::Rdp,
            ProtocolKind::Vnc => Protocol::Vnc,
            _ => return,
        };
        // 表单用户名可能是保存时合并的 `DOMAIN\user`，打开时拆成两个字段。
        let (domain, username) = match host.user.split_once('\\') {
            Some((domain, user)) => (domain.to_string(), user.to_string()),
            None => (String::new(), host.user.clone()),
        };
        self.new_desktop_with(
            Some(DesktopPrefill {
                protocol,
                host: host.host.clone(),
                port: host.port,
                username,
                domain,
                // 降级模式没有稳定主键（全为 0），不按 id 回写，避免更新错行。
                saved_host: (host.id != 0).then_some(host.id),
                auto_connect: true,
            }),
            window,
            cx,
        );
    }

    pub(super) fn new_desktop_with(
        &mut self,
        prefill: Option<DesktopPrefill>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        self.view_split = None;
        let protocol = prefill
            .as_ref()
            .map(|prefill| prefill.protocol)
            .unwrap_or(Protocol::Vnc);
        let prefilled_host = prefill.as_ref().map(|prefill| prefill.host.clone());
        let port_text = prefill
            .as_ref()
            .map(|prefill| prefill.port.to_string())
            .unwrap_or_else(|| protocol.default_port().to_string());
        let prefilled_username = prefill
            .as_ref()
            .map(|prefill| prefill.username.clone())
            .unwrap_or_default();
        let prefilled_domain = prefill
            .as_ref()
            .map(|prefill| prefill.domain.clone())
            .unwrap_or_default();
        let saved_host = prefill.as_ref().and_then(|prefill| prefill.saved_host);
        let auto_connect = prefill.as_ref().is_some_and(|prefill| prefill.auto_connect);
        // 已保存主机回填原分组；新建默认未分组。
        let group = saved_host
            .and_then(|id| self.sidebar.group_of(id))
            .map(SharedString::from);
        let form_host = cx.new(|cx| {
            let input =
                InputState::new(window, cx).placeholder(tr!(cx, "主机或 IP（可写 host:port）"));
            match &prefilled_host {
                Some(host) => input.default_value(host.clone()),
                None => input,
            }
        });
        let form_port = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(port_text.clone())
                .placeholder(port_text.clone())
        });
        let form_password =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "密码（可留空）")));
        let form_username = cx.new(|cx| {
            let input = InputState::new(window, cx).placeholder(tr!(cx, "用户名"));
            if prefilled_username.is_empty() {
                input
            } else {
                input.default_value(prefilled_username.clone())
            }
        });
        let form_domain = cx.new(|cx| {
            let input = InputState::new(window, cx).placeholder(tr!(cx, "域（可留空）"));
            if prefilled_domain.is_empty() {
                input
            } else {
                input.default_value(prefilled_domain.clone())
            }
        });
        // 表单里按回车直接开连，省一次鼠标。
        for input in [
            &form_host,
            &form_port,
            &form_password,
            &form_username,
            &form_domain,
        ] {
            let connect_id = id;
            cx.subscribe(input, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    if auto_connect {
                        // 直连标签只显示密码框，回车＝提交密码。
                        this.submit_desktop_password(connect_id, cx);
                    } else {
                        this.begin_desktop_connect(connect_id, cx);
                    }
                }
            })
            .detach();
        }
        // 画面失焦就释放远端按键：切走窗口 / 点别处时，远端不该还按着 Ctrl 或鼠标。
        let focus = cx.focus_handle();
        let entity = cx.entity();
        let focus_out = window.on_focus_out(&focus, cx, move |_, _, cx| {
            entity.update(cx, |this, cx| this.desktop_focus_lost(id, cx));
        });
        let desktop = DesktopTab {
            host: prefilled_host.clone().unwrap_or_default(),
            port: port_text.parse::<u16>().unwrap_or(protocol.default_port()),
            protocol,
            saved_host,
            direct: auto_connect,
            cache_key: None,
            group,
            password: String::new(),
            username: prefilled_username,
            domain: prefilled_domain,
            request_size: (1280, 800),
            shared: true,
            phase: DesktopPhase::Form,
            form_host,
            form_port,
            form_password,
            form_username,
            form_domain,
            save_secret: false,
            form_error: None,
            focus,
            image: None,
            drop_queue: Vec::new(),
            draw_rect: Rc::new(Cell::new(None)),
            screen_bounds: Rc::new(Cell::new(None)),
            size: (0, 0),
            name: String::new(),
            zoom: 0.0,
            capture_keys: false,
            modifiers: Modifiers::default(),
            capslock: false,
            buttons: 0,
            command: None,
            latest: None,
            upload: None,
            fps: 0.0,
            token: 0,
            clipboard_token: None,
            _focus_out: Some(focus_out),
        };
        // 直连标签开局就带地址，别在等密码时还写着泛泛的「远程桌面」。
        let initial_title = if auto_connect {
            prefilled_host
                .as_deref()
                .filter(|host| !host.is_empty())
                .map(|host| {
                    SharedString::from(host_port(
                        host,
                        port_text.parse::<u16>().unwrap_or(protocol.default_port()),
                    ))
                })
                .unwrap_or_else(|| tr!(cx, "远程桌面"))
        } else {
            tr!(cx, "远程桌面")
        };
        let title_ordinal = self.next_title_ordinal(&initial_title, None);
        self.tabs.push(SessionTab {
            id,
            title: initial_title.clone(),
            base_title: initial_title,
            custom_title: None,
            title_ordinal,
            follow_osc_title: false,
            addr: SharedString::default(),
            auth: None,
            remote_info: None,
            platform: None,
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
            zmodem_cancel: None,
            auto_retry_attempt: 0,
            auto_retry_pending: false,
            connected_at: None,
        });
        self.active_tab = Some(id);
        self.activate_tab(id, window, cx);
        if !auto_connect {
            self.status = if saved_host.is_some() {
                tr!(cx, "已填入主机信息，输入密码后点「连接」")
            } else if prefilled_host
                .as_deref()
                .is_some_and(|host| !host.is_empty())
            {
                tr!(cx, "已填入发现的地址，填写凭据后点「连接」")
            } else {
                tr!(cx, "填写地址后按回车或点「连接」")
            };
        }
        self.sync_tray(cx);
        cx.notify();
        if auto_connect {
            self.auto_connect_desktop(id, window, cx);
        }
    }

    /// 已保存主机的直连入口：本次内存缓存 → 加密库 → 按协议决定问密码还是免密试连。
    fn auto_connect_desktop(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
        else {
            return;
        };
        let (host, port, protocol, username, domain, saved_host) = (
            desktop.host.clone(),
            desktop.port,
            desktop.protocol,
            desktop.username.clone(),
            desktop.domain.clone(),
            desktop.saved_host,
        );
        if host.is_empty() {
            self.set_desktop_form(id, cx);
            self.desktop_form_error(id, tr!(cx, "远程桌面：请填写主机地址"), cx);
            return;
        }
        let key = desktop_cache_key(&host, port, &username, &domain);
        if let Some(password) = self.password_cache.get(&key).cloned() {
            if let Some(desktop) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == id)
                .and_then(|tab| tab.desktop.as_mut())
            {
                desktop.form_password.update(cx, |state, cx| {
                    state.set_value(password.clone(), window, cx)
                });
            }
            self.open_desktop_session(id, host, port, password, cx);
            return;
        }
        // 加密库里存过密码（保存主机或上次连接成功时写入）：读出来直接用，重启后也不再问。
        if let (Some(vault), Some(host_id)) = (self.vault.clone(), saved_host.filter(|id| *id != 0))
        {
            if let Some(desktop) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == id)
                .and_then(|tab| tab.desktop.as_mut())
            {
                // 读库是异步的：先停在「连接中」，别让完整表单闪一下。
                desktop.phase = DesktopPhase::Connecting;
            }
            if self.active_tab == Some(id) {
                self.status = tr!(cx, "正在读取已保存的密码…");
            }
            cx.notify();
            let entity = cx.entity();
            let entity_id = entity.entity_id();
            cx.spawn(async move |_, cx| {
                let saved = vault.password_of(host_id).await.ok().flatten();
                let _ = cx.with_window(entity_id, move |window, cx| {
                    entity.update(cx, move |this, cx| {
                        this.finish_desktop_direct(
                            id,
                            host,
                            port,
                            protocol,
                            username,
                            saved.as_deref(),
                            window,
                            cx,
                        );
                    });
                });
            })
            .detach();
            return;
        }
        self.finish_desktop_direct(id, host, port, protocol, username, None, window, cx);
    }

    /// 直连收尾：有密码就带着连；没有则 RDP 只问密码，VNC 先试免密。
    #[allow(clippy::too_many_arguments)]
    fn finish_desktop_direct(
        &mut self,
        id: usize,
        host: String,
        port: u16,
        protocol: Protocol,
        username: String,
        password: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 读库期间标签可能已关、或用户已手动发起连接：迟到的结果直接丢弃。
        let ready = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
            .is_some_and(|desktop| {
                desktop.direct
                    && desktop.command.is_none()
                    && matches!(desktop.phase, DesktopPhase::Form | DesktopPhase::Connecting)
            });
        if !ready {
            return;
        }
        if let Some(password) = password.filter(|password| !password.is_empty()) {
            let password = password.to_string();
            if let Some(desktop) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == id)
                .and_then(|tab| tab.desktop.as_mut())
            {
                desktop.form_password.update(cx, |state, cx| {
                    state.set_value(password.clone(), window, cx)
                });
            }
            self.open_desktop_session(id, host, port, password, cx);
            return;
        }
        if protocol == Protocol::Rdp {
            // 缺账号就只能回完整表单；账号齐全才走「只问密码」。
            if username.is_empty() {
                self.set_desktop_form(id, cx);
                self.desktop_form_error(id, tr!(cx, "RDP：请填写用户名"), cx);
                return;
            }
            if let Some(desktop) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == id)
                .and_then(|tab| tab.desktop.as_mut())
            {
                desktop.phase = DesktopPhase::NeedPassword;
                desktop.form_error = None;
                let focus = desktop.form_password.read(cx).focus_handle(cx);
                focus.focus(window, cx);
            }
            if self.active_tab == Some(id) {
                self.status = tr!(cx, "请输入远程桌面密码后连接");
            }
            cx.notify();
            return;
        }
        self.open_desktop_session(id, host, port, String::new(), cx);
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
        let username = desktop.form_username.read(cx).value().trim().to_string();
        let domain = desktop.form_domain.read(cx).value().trim().to_string();
        let protocol = desktop.protocol;
        let (host, port) = normalize_address(&raw, &port_text);
        if host.is_empty() {
            self.desktop_form_error(id, tr!(cx, "远程桌面：请填写主机地址"), cx);
            return;
        }
        let Some(port) = port else {
            self.desktop_form_error(id, tr!(cx, "远程桌面：端口无效"), cx);
            return;
        };
        // RDP 需要 Windows 账户名；密码允许留空，由服务器判定（认证失败会回到表单）。
        if protocol == Protocol::Rdp && username.is_empty() {
            self.desktop_form_error(id, tr!(cx, "RDP：请填写用户名"), cx);
            return;
        }
        if let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        {
            desktop.username = username;
            desktop.domain = domain;
            desktop.form_error = None;
        }
        self.open_desktop_session(id, host, port, password, cx);
    }

    /// 直连标签的密码提交：只看密码框（可留空），地址与账号沿用已保存主机。
    pub(super) fn submit_desktop_password(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
        else {
            return;
        };
        let password = desktop.form_password.read(cx).value().to_string();
        let (host, port) = (desktop.host.clone(), desktop.port);
        let missing_user = desktop.protocol == Protocol::Rdp && desktop.username.is_empty();
        if host.is_empty() {
            self.set_desktop_form(id, cx);
            self.desktop_form_error(id, tr!(cx, "远程桌面：请填写主机地址"), cx);
            return;
        }
        if missing_user {
            self.set_desktop_form(id, cx);
            self.desktop_form_error(id, tr!(cx, "RDP：请填写用户名"), cx);
            return;
        }
        if let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        {
            desktop.password = password.clone();
            desktop.form_error = None;
        }
        self.open_desktop_session(id, host, port, password, cx);
    }

    /// 表单校验失败的统一出口：错误同时进表单与状态栏，标签不离开表单。
    fn desktop_form_error(&mut self, id: usize, message: SharedString, cx: &mut Context<Self>) {
        if let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        {
            desktop.form_error = Some(message.clone());
        }
        if self.active_tab == Some(id) {
            self.status = message;
        }
        cx.notify();
    }

    /// 表单「取消」：直接关掉这个桌面标签（尚未建连，不算断开）。
    pub(super) fn cancel_desktop_form(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        self.close_tab(tab_id, cx);
        self.status = tr!(cx, "已取消远程桌面");
    }

    /// 表单「保存」：把地址 / 端口 / 协议 / 用户名存成常用主机；填了密码则连同加密入库。
    pub(super) fn save_desktop_host(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.desktop.as_ref())
        else {
            return;
        };
        let raw = desktop.form_host.read(cx).value().trim().to_string();
        let port_text = desktop.form_port.read(cx).value().trim().to_string();
        let password = desktop.form_password.read(cx).value().to_string();
        let protocol = match desktop.protocol {
            Protocol::Rdp => ProtocolKind::Rdp,
            Protocol::Vnc => ProtocolKind::Vnc,
        };
        let username = desktop.form_username.read(cx).value().trim().to_string();
        let domain = desktop.form_domain.read(cx).value().trim().to_string();
        let saved_id = desktop.saved_host;
        let group = desktop.group.clone();
        if raw.is_empty() {
            self.desktop_form_error(tab_id, tr!(cx, "请填写主机地址"), cx);
            return;
        }
        // 地址里可能带着端口，落库前拆开，否则保存的 host 会变成 `host:port`。
        let (address, parsed_port) = normalize_address(&raw, &port_text);
        if address.is_empty() {
            self.desktop_form_error(tab_id, tr!(cx, "请填写主机地址"), cx);
            return;
        }
        let Some(port) = parsed_port else {
            self.desktop_form_error(tab_id, tr!(cx, "端口无效"), cx);
            return;
        };
        // 域并进用户名（`DOMAIN\user`）：库表没有单独的域列，Windows 本身也认这种写法。
        let user = if domain.is_empty() || username.contains('\\') || username.contains('@') {
            username
        } else if username.is_empty() {
            domain
        } else {
            format!("{domain}\\{username}")
        };
        // 保存时填的密码进内存缓存（本进程内点主机直接复用）；凭据库可用时还会加密入库。
        // 键用合并后的 `user`，与打开已保存主机时拆分再合并的结果一致。
        let has_password = !password.is_empty();
        let save_secret = has_password && desktop.save_secret;
        let cached = has_password.then(|| {
            (
                desktop_cache_key(&address, port, &user, ""),
                password.clone(),
            )
        });
        let existing = saved_id.and_then(|id| self.hosts.iter().position(|host| host.id == id));
        let mut host = match existing {
            Some(index) => {
                let mut host = self.hosts[index].clone();
                host.protocol = protocol;
                host.host = address;
                host.port = port;
                host.user = user;
                // 桌面主机不使用 SSH 认证材料；跳板机同样不适用。
                host.auth = AuthMethod::Agent;
                host.key_path.clear();
                host.key_inline = false;
                host.proxy_jump = None;
                host
            }
            None => HostConfig {
                protocol,
                id: 0,
                name: String::new(),
                host: address,
                port,
                user,
                auth: AuthMethod::Agent,
                key_path: String::new(),
                key_inline: false,
                favorite: false,
                proxy_jump: None,
            },
        };
        if let Some(error) = host.validate() {
            self.desktop_form_error(tab_id, SharedString::from(format!("保存失败：{error}")), cx);
            return;
        }
        let label = format!("{}:{}", host.host, host.port);
        let group_note = group
            .as_deref()
            .map(|name| format!("（分组：{name}）"))
            .unwrap_or_default();
        match existing {
            Some(index) => {
                self.hosts[index] = host.clone();
                self.persist_host(&host, cx);
                self.note_protocol(&host, cx);
            }
            None => {
                if self.vault.is_some() {
                    host.id = self.hosts.iter().map(|item| item.id).max().unwrap_or(0) + 1;
                }
                self.hosts.push(host.clone());
                self.persist_host(&host, cx);
                self.note_protocol(&host, cx);
            }
        }
        self.apply_form_group(&host, group, cx);
        if let Some((key, password)) = cached {
            self.password_cache.insert(key, password);
        }
        // 仅在用户勾选「保存密码」时才加密写库：下次（包括重启后）点这台主机直接连，不再追问。
        let stored = save_secret
            && store_desktop_secret_after_save(self.vault.clone(), host.clone(), &password, cx);
        let password_note = if stored {
            "（密码已加密入库）"
        } else if has_password {
            "（密码仅本次内存持有）"
        } else {
            ""
        };
        // 保存＝创建/更新完成：关掉这张表单标签，回到列表（要连接再点该主机）。
        self.close_tab(tab_id, cx);
        self.status = SharedString::from(format!("已保存主机 {label}{group_note}{password_note}"));
        self.sync_tray(cx);
        cx.notify();
    }

    /// 表单里切换协议：端口还是上一个协议的默认值（或为空）时顺手换成新默认端口。
    pub(super) fn set_desktop_protocol(
        &mut self,
        id: usize,
        protocol: Protocol,
        window: &mut Window,
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
        if desktop.protocol == protocol {
            return;
        }
        let previous = desktop.protocol.default_port().to_string();
        desktop.protocol = protocol;
        desktop.form_error = None;
        let port = desktop.form_port.read(cx).value().trim().to_string();
        if port.is_empty() || port == previous {
            let next = protocol.default_port().to_string();
            desktop
                .form_port
                .update(cx, |state, cx| state.set_value(next, window, cx));
        }
        cx.notify();
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
            // 直连标签取消后回到密码框，而不是完整表单。
            desktop.phase = if desktop.direct {
                DesktopPhase::NeedPassword
            } else {
                DesktopPhase::Form
            };
            desktop.command = None;
            desktop.latest = None;
            desktop.clear_buttons();
        }
        if self.active_tab == Some(id) {
            self.status = tr!(cx, "已取消连接");
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
        let (host, port, password) = (desktop.host.clone(), desktop.port, desktop.password.clone());
        self.open_desktop_session(id, host, port, password, cx);
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
            // 「修改参数」进入完整表单：之后保存 / 连接都按表单语义走。
            desktop.direct = false;
            cx.notify();
        }
    }

    fn open_desktop_session(
        &mut self,
        id: usize,
        host: String,
        port: u16,
        password: String,
        cx: &mut Context<Self>,
    ) {
        let addr = host_port(&host, port);
        let title = SharedString::from(addr.clone());
        // 标题从「远程桌面」换成真实地址时重新取号：只有同地址的标签才编号。
        let title_ordinal = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .filter(|tab| tab.base_title != title)
            .map(|_| self.next_title_ordinal(&title, Some(id)));
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
        desktop.cache_key = Some(desktop_cache_key(
            &host,
            port,
            &desktop.username,
            &desktop.domain,
        ));
        desktop.password = password.clone();
        desktop.phase = DesktopPhase::Connecting;
        desktop.command = None;
        desktop.latest = None;
        desktop.fps = 0.0;
        desktop.clear_buttons();
        release_desktop_clipboard(desktop);
        if let Some(old) = desktop.image.take() {
            desktop.drop_queue.push(old);
        }
        tab.ended = false;
        tab.failed = false;
        tab.last_error = None;
        tab.addr = addr.clone().into();
        if tab.custom_title.is_none() {
            if let Some(ordinal) = title_ordinal {
                tab.title_ordinal = ordinal;
            }
            tab.title = title.clone();
            tab.base_title = title;
        }
        let active = self.active_tab == Some(id);
        if active {
            self.status = tr!(cx, "正在连接远程桌面 {addr}…", addr = addr);
        }
        let config = crate::desktop::Config {
            protocol: desktop.protocol,
            host,
            port,
            password: (!password.is_empty()).then_some(password),
            shared: desktop.shared,
            username: (!desktop.username.is_empty()).then(|| desktop.username.clone()),
            domain: (!desktop.domain.is_empty()).then(|| desktop.domain.clone()),
            size: desktop.request_size,
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
            clipboard_token,
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
        desktop.clipboard_token = clipboard_token;
        // 连接成功前不切 Live、不缓存密码：VNC 帧到达或 RDP 首帧才真正可用。
        tab.ended = false;
        tab.failed = false;
        tab.last_error = None;
        // 活动标签立即拿到剪贴板授权；后台标签等切过去时再接。
        if self.active_tab == Some(id)
            && let Some(token) = clipboard_token
        {
            ironrdp_cliprdr_native::authorize_clipboard_session(token);
        }
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
                        let snapshot = latest_slot.lock().ok().and_then(|mut slot| slot.take());
                        match snapshot {
                            Some(snapshot) => {
                                let size = (snapshot.width, snapshot.height);
                                let image = snapshot_to_image(snapshot);
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
                    Event::Upload {
                        done,
                        total,
                        files,
                        finished,
                        failed,
                    } => this
                        .update(cx, |this, cx| {
                            this.apply_desktop_upload(
                                id, token, done, total, files, finished, failed, cx,
                            );
                        })
                        .is_ok(),
                    Event::Clipboard(text) => this
                        .update(cx, |this, cx| {
                            this.apply_desktop_clipboard(id, token, text, cx)
                        })
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
            // 界面长时间不渲染（最小化 / 完全遮挡）时队列只进不出，设上限就地
            // 释放最老的帧。本回调跑在 App 上下文、不在任何窗口的更新周期里，
            // `drop_image(.., None)` 能清到所有窗口的精灵图集；对从未绘制过的
            // 帧是无害 no-op（图集里本来就没有它）。
            while desktop.drop_queue.len() > DESKTOP_DROP_QUEUE_CAP {
                let oldest = desktop.drop_queue.remove(0);
                cx.drop_image(oldest, None);
            }
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
        let title = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
            .map(|desktop| SharedString::from(host_port(&desktop.host, desktop.port)));
        // 标题从「远程桌面」换成真实地址时重新取号：只有同地址的标签才编号。
        let title_ordinal = title.as_ref().and_then(|title| {
            self.tabs
                .iter()
                .find(|tab| tab.id == id)
                .filter(|tab| tab.base_title != *title)
                .map(|_| self.next_title_ordinal(title, Some(id)))
        });
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let Some(desktop) = tab.desktop.as_mut() else {
            return;
        };
        if desktop.token != token {
            return;
        }
        desktop.name = name;
        desktop.size = (width, height);
        // 重复的 Connected（重绘/重协商）只更新尺寸，不重复缓存密码。
        if desktop.phase == DesktopPhase::Live {
            cx.notify();
            return;
        }
        desktop.phase = DesktopPhase::Live;
        desktop.capture_keys = true;
        // 连接成功才缓存密码：内存里供本进程复用；已保存主机再加密写入凭据库，
        // 重启后点同一主机也不用再问。
        let connected_password = desktop.password.clone();
        let cached_password = if connected_password.is_empty() {
            None
        } else {
            desktop
                .cache_key
                .clone()
                .map(|key| (key, connected_password.clone()))
        };
        let saved_host = desktop.saved_host;
        // 连接成功也只在该主机已勾选「保存密码」（或来自已保存主机）时才回写密码库。
        let save_secret = desktop.save_secret;
        tab.connected_at = Some(std::time::Instant::now());
        if tab.custom_title.is_none()
            && let Some(title) = title
        {
            if let Some(ordinal) = title_ordinal {
                tab.title_ordinal = ordinal;
            }
            tab.title = title.clone();
            tab.base_title = title;
        }
        if let Some((key, password)) = cached_password {
            self.password_cache.insert(key, password);
        }
        if save_secret {
            store_desktop_secret(self.vault.clone(), saved_host, &connected_password, cx);
        }
        self.status = tr!(cx, "已连接远程桌面");
        self.sync_tray(cx);
        cx.notify();
    }

    fn apply_desktop_fps(&mut self, id: usize, token: u64, fps: f32, cx: &mut Context<Self>) {
        if let Some(desktop) = self
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

    /// 拖放上传进度：传输中刷新进度条，结束（成功或中断）时收起。
    #[allow(clippy::too_many_arguments)]
    fn apply_desktop_upload(
        &mut self,
        id: usize,
        token: u64,
        done: u64,
        total: u64,
        files: u32,
        finished: bool,
        failed: bool,
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
        if finished {
            desktop.upload = None;
            if self.active_tab == Some(id) {
                if failed {
                    self.status = if total == 0 {
                        tr!(
                            cx,
                            "文件粘贴未开始（本机剪贴板被占用或远端未接收文件列表），可稍后重试；远端内容就绪时可手动按 Ctrl+V"
                        )
                    } else if done == 0 {
                        // 远端收下了列表却一个字节都没读：最常见就是同名文件覆盖确认框在等人。
                        tr!(
                            cx,
                            "远端还没开始接收：若弹出了同名文件覆盖确认，请在那个窗口里选「替换」，或先删掉远端同名文件"
                        )
                    } else {
                        format!(
                            "{}：{} / {}",
                            tr!(cx, "文件传输中断"),
                            crate::transfer::format_bytes(done),
                            crate::transfer::format_bytes(total)
                        )
                        .into()
                    };
                } else if total > 0 {
                    self.status = tr!(
                        cx,
                        "文件已发送到远端：{size}",
                        size = crate::transfer::format_bytes(total)
                    );
                }
            }
        } else {
            desktop.upload = Some(UploadStatus { done, total, files });
        }
        cx.notify();
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
        // 只有活动标签的会话才写本机剪贴板：后台 VNC 标签若也照写，两个
        // VNC 会话会互相覆盖用户正在用的剪贴板。RDP 侧用 clipboard_token
        // 保证了这一点，VNC 的 token 恒为 None，这里补上同一约束。
        if self.active_tab != Some(id) {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status = tr!(cx, "已收到远端剪贴板");
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
        // 认证类失败直接回输入处：直连标签回密码框，表单标签回完整表单，
        // 用户改密码就能重试，不必去失败卡片上找「修改参数」。
        let auth_failure = desktop_auth_failure(desktop.protocol, &message);
        let message = SharedString::from(message);
        desktop.command = None;
        desktop.latest = None;
        desktop.capture_keys = false;
        desktop.clear_buttons();
        release_desktop_clipboard(desktop);
        if auth_failure {
            // 缓存里留下的若是旧密码，认证失败后必须清掉，否则下次点主机又拿它重试。
            if let Some(key) = desktop.cache_key.take() {
                self.password_cache.remove(&key);
            }
            // 库里存的同样作废，否则重启后会拿旧密码直连。
            clear_desktop_secret(self.vault.clone(), desktop.saved_host, cx);
            desktop.password.clear();
            desktop.phase = if desktop.direct {
                DesktopPhase::NeedPassword
            } else {
                DesktopPhase::Form
            };
            desktop.form_error = Some(if desktop.direct {
                tr!(cx, "认证失败：请重新输入密码后重试（可留空再试）")
            } else if desktop.protocol == Protocol::Rdp {
                tr!(cx, "RDP 认证失败：请检查用户名、域和密码后重试")
            } else {
                tr!(cx, "VNC 认证失败：请检查密码后重试")
            });
            tab.ended = false;
            tab.failed = false;
        } else {
            desktop.phase = DesktopPhase::Failed {
                message: message.clone(),
            };
            tab.ended = true;
            tab.failed = true;
        }
        tab.last_error = Some(message.clone());
        if self.active_tab == Some(id) {
            self.status = if auth_failure {
                desktop
                    .form_error
                    .clone()
                    .unwrap_or_else(|| message.clone())
            } else {
                message
            };
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
        // 认证失败有时是连上后立刻被服务器掐断（而不是建连报错）：同样退回表单，
        // 让用户改密码重试，而不是停在失败卡片上。
        let auth_failure = desktop_auth_failure(desktop.protocol, &reason);
        let reason = SharedString::from(reason);
        desktop.command = None;
        desktop.latest = None;
        desktop.capture_keys = false;
        desktop.clear_buttons();
        release_desktop_clipboard(desktop);
        if auth_failure {
            // 同 fail_desktop：清掉失效的缓存密码，避免下次直连再拿旧密码重试。
            if let Some(key) = desktop.cache_key.take() {
                self.password_cache.remove(&key);
            }
            clear_desktop_secret(self.vault.clone(), desktop.saved_host, cx);
            desktop.password.clear();
            desktop.phase = if desktop.direct {
                DesktopPhase::NeedPassword
            } else {
                DesktopPhase::Form
            };
            desktop.form_error = Some(if desktop.direct {
                tr!(cx, "认证失败：请重新输入密码后重试（可留空再试）")
            } else if desktop.protocol == Protocol::Rdp {
                tr!(cx, "RDP 认证失败：请检查用户名、域和密码后重试")
            } else {
                tr!(cx, "VNC 认证失败：请检查密码后重试")
            });
            tab.ended = false;
        } else {
            desktop.phase = DesktopPhase::Failed {
                message: reason.clone(),
            };
            tab.ended = true;
        }
        tab.failed = false;
        tab.last_error = Some(reason.clone());
        if self.active_tab == Some(id) {
            self.status = if auth_failure {
                desktop.form_error.clone().unwrap_or_else(|| reason.clone())
            } else {
                reason
            };
        }
        self.sync_tray(cx);
        cx.notify();
    }

    /// 拖文件到远程桌面标签：RDP 走协议自带的文件剪贴板，不需要远端装 SSH。
    ///
    /// 文件先放进本机剪贴板，剪贴板线程的监听窗口会把它报给远端；稍后会话替
    /// 用户按一次 Ctrl+V，资源管理器就落到当前文件夹。VNC 的 RFB 没有文件通道，
    /// 直接说明不支持，不再要求另开 SSH 会话。
    pub(super) fn desktop_drop_paths(
        &mut self,
        id: usize,
        paths: Vec<PathBuf>,
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
        let protocol = desktop.protocol;
        let live = desktop.phase == DesktopPhase::Live && desktop.command.is_some();
        if protocol == Protocol::Vnc {
            self.status = tr!(cx, "VNC 协议没有文件传输能力：需要传文件请改用 RDP 连接");
            cx.notify();
            return;
        }
        if !live {
            self.status = tr!(cx, "远程桌面还没连上：连上后再拖入文件");
            cx.notify();
            return;
        }
        let count = paths.len();
        desktop.send(Command::PasteFiles(paths));
        self.status = tr!(
            cx,
            "已把 {count} 个文件放进剪贴板并在远端粘贴到当前文件夹；没反应时在远端按 Ctrl+V",
            count = count
        );
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
        desktop.send(Command::ReleaseAll);
        desktop.send(Command::Close);
        desktop.capture_keys = false;
        desktop.clear_buttons();
        release_desktop_clipboard(desktop);
        if self.active_tab == Some(id) {
            self.status = tr!(cx, "正在断开远程桌面…");
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

    fn set_desktop_button(
        &mut self,
        id: usize,
        button: MouseButton,
        down: bool,
        cx: &mut Context<Self>,
    ) {
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

    fn send_desktop_wheel(
        &mut self,
        id: usize,
        up: bool,
        position: Point<Pixels>,
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
        let Some(rect) = desktop.draw_rect.get() else {
            return;
        };
        let Some((x, y)) = remote_point(rect, desktop.size, position) else {
            return;
        };
        desktop.send(Command::Wheel {
            x,
            y,
            up,
            buttons: desktop.buttons,
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
        if let Some(key) = KeyInput::from_event(&event.key, event.key_char.as_deref()) {
            desktop.send(Command::Key { key, down });
        }
        let _ = cx;
    }

    /// 画面失焦：释放远端所有按键与鼠标按钮，避免切走窗口后“黏键”。
    fn desktop_focus_lost(&mut self, id: usize, cx: &mut Context<Self>) {
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
        desktop.send(Command::ReleaseAll);
        desktop.capture_keys = false;
        desktop.modifiers = Modifiers::default();
        desktop.capslock = false;
        desktop.clear_buttons();
        if self.active_tab == Some(id) {
            self.status = tr!(cx, "远程桌面失去焦点，已释放键盘");
        }
        cx.notify();
    }

    /// 当前标签是否正在捕获键盘（活动标签是桌面、画面持有焦点且捕获已开启）。
    ///
    /// 用于全局按键拦截：捕获期间应用快捷键要让位给远端。
    pub(super) fn desktop_capturing(&self, window: &Window) -> bool {
        self.active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .and_then(|tab| tab.desktop.as_ref())
            .is_some_and(|desktop| desktop.capture_keys && desktop.focus.is_focused(window))
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
                // 释放捕获前把远端还按着的键与鼠标抬起来，否则远端会一直认为 Ctrl 没松。
                desktop.send(Command::ReleaseAll);
                desktop.modifiers = Modifiers::default();
                desktop.capslock = false;
                desktop.clear_buttons();
                desktop.capture_keys = false;
            }
            self.status = tr!(cx, "已释放键盘，点击画面可重新捕获");
            cx.notify();
            return;
        }
        // VNC 没有原生剪贴板同步之外的手工通道：按 Ctrl+V 时先把本地文本推过去，
        // 再把按键原样转发，远端粘贴到的就是刚复制的内容。
        let vnc_paste = desktop.protocol == Protocol::Vnc
            && event.keystroke.key.eq_ignore_ascii_case("v")
            && event.keystroke.modifiers.control;
        if vnc_paste {
            self.sync_vnc_clipboard(id, cx);
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

    /// 修饰键 / CapsLock 变化：按与上次的差异补发按下 / 抬起。
    ///
    /// rgpui（与 gpui 一致）不为 Ctrl / Shift / Alt / Win 发 KeyDown / KeyUp，
    /// 只发这一个事件；不在这里转发，远端就永远收不到 Ctrl+V 的 Ctrl。
    pub(super) fn desktop_modifiers_changed(
        &mut self,
        id: usize,
        event: &ModifiersChangedEvent,
        window: &mut Window,
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
        if !desktop.capture_keys || !desktop.focus.is_focused(window) {
            return;
        }
        cx.stop_propagation();
        let next = event.modifiers;
        let previous = std::mem::replace(&mut desktop.modifiers, next);
        for (name, before, now) in [
            ("shift", previous.shift, next.shift),
            ("control", previous.control, next.control),
            ("alt", previous.alt, next.alt),
            ("super", previous.platform, next.platform),
        ] {
            if before != now {
                desktop.send(Command::Key {
                    key: KeyInput::modifier(name),
                    down: now,
                });
            }
        }
        // CapsLock 是切换键：远端要一按一抬才能翻转状态，不能只发按下。
        if desktop.capslock != event.capslock.on {
            desktop.capslock = event.capslock.on;
            let key = KeyInput::modifier("capslock");
            desktop.send(Command::Key { key, down: true });
            desktop.send(Command::Key { key, down: false });
        }
    }

    /// 工具条：把本地剪贴板文本发给远端。
    fn send_clipboard_to_desktop(&mut self, id: usize, cx: &mut Context<Self>) {
        let protocol = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_ref())
            .map(|desktop| desktop.protocol);
        if protocol == Some(Protocol::Rdp) {
            // IronRDP 的 WinClipboard 后端直接挂系统剪贴板，文本与文件在会话期间自动双向同步；
            // 这里再手工发一遍反而会覆盖远端刚复制的内容。
            self.status = tr!(
                cx,
                "RDP 剪贴板已随系统自动同步，直接在远端按 Ctrl+V 即可（文件也可直接粘贴）"
            );
            cx.notify();
            return;
        }
        self.sync_vnc_clipboard(id, cx);
    }

    /// 把本地剪贴板文本推给 VNC 会话（工具条按钮与 Ctrl+V 共用）。
    fn sync_vnc_clipboard(&mut self, id: usize, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|item| {
            item.entries.into_iter().find_map(|entry| match entry {
                ClipboardEntry::String(text) => Some(text.text),
                _ => None,
            })
        });
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            self.status = tr!(cx, "剪贴板里没有文本");
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
        self.status = tr!(cx, "已把剪贴板发送到远端");
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

    /// 让 RDP 把远端桌面分辨率改成当前窗口大小（内置远程桌面的默认行为）。
    fn match_desktop_window(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(desktop) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.desktop.as_mut())
        else {
            return;
        };
        let Some(bounds) = desktop.screen_bounds.get() else {
            self.status = tr!(cx, "窗口还没完成布局，稍后再试");
            cx.notify();
            return;
        };
        let scale = window.scale_factor().max(1.0);
        // 远端按物理像素建会话；偶数宽高避免缩放时半像素发虚。
        let width = ((bounds.size.width.as_f32() * scale).round() as u32).clamp(200, 8192) as u16;
        let height = ((bounds.size.height.as_f32() * scale).round() as u32).clamp(200, 8192) as u16;
        let (width, height) = (width & !1, height & !1);
        desktop.zoom = 0.0;
        // 记住这次尺寸：断线重连时按同一个分辨率建会话，少一次二次 resize。
        desktop.request_size = (width, height);
        desktop.send(Command::Resize {
            width,
            height,
            scale_factor: (scale * 100.0).round() as u32,
        });
        self.status = tr!(
            cx,
            "正在把远端桌面调整为 {width}×{height}…",
            width = width,
            height = height
        );
        cx.notify();
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
        let on = !desktop.capture_keys;
        desktop.capture_keys = on;
        if on {
            desktop.focus.focus(window, cx);
        } else {
            // 关掉捕获时把远端还按着的键与鼠标一并抬起。
            desktop.send(Command::ReleaseAll);
            desktop.modifiers = Modifiers::default();
            desktop.capslock = false;
            desktop.clear_buttons();
        }
        self.status = if on {
            tr!(cx, "键盘已捕获（Esc 释放）")
        } else {
            tr!(cx, "已释放键盘")
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
            DesktopPhase::NeedPassword => self.render_desktop_password(tab.id, desktop, cx),
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
                let screen = self.render_desktop_screen(id, desktop, cx);
                let upload = self.render_upload_card(desktop, cx);
                v_flex()
                    .size_full()
                    .bg(pal.window_bg)
                    .child(self.render_desktop_toolbar(tab, desktop, cx))
                    .child(
                        // 必须是 flex 容器：画面根节点靠 flex_1 吃满剩余高度，
                        // 换成普通 div 会让 canvas 变成 0 高（整片白屏）。
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .child(screen)
                            .when_some(upload, |this, card| this.child(card)),
                    )
                    .into_any_element()
            }
        }
    }

    /// 右下角的拖放上传进度卡：文件数 + 已发送字节 + 进度条。
    fn render_upload_card(
        &self,
        desktop: &DesktopTab,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let upload = desktop.upload.as_ref()?;
        let pal = self.palette(cx);
        let fraction = if upload.total > 0 {
            (upload.done as f32 / upload.total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let detail = format!(
            "{} / {}",
            crate::transfer::format_bytes(upload.done),
            crate::transfer::format_bytes(upload.total)
        );
        let title = tr!(cx, "正在粘贴 {count} 个文件到远端", count = upload.files);
        Some(
            v_flex()
                .absolute()
                .bottom(px(12.0))
                .right(px(12.0))
                .w(px(260.0))
                .gap_1()
                .px_3()
                .py_2()
                .rounded(px(6.0))
                .bg(pal.card_bg)
                .border_1()
                .border_color(pal.border)
                .shadow_md()
                .child(div().text_xs().text_color(pal.text).child(title))
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
                )
                .child(div().text_xs().text_color(pal.muted).child(detail))
                .into_any_element(),
        )
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
        let bounds_sink = desktop.screen_bounds.clone();
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
                        // 捕获瞬间以窗口的真实修饰键状态为基线：之后只补发变化量，
                        // 不会把点击前就按着的键误发一遍。
                        desktop.modifiers = window.modifiers();
                        desktop.capslock = window.capslock().on;
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
            .on_modifiers_changed(cx.listener(
                move |this, event: &ModifiersChangedEvent, window, cx| {
                    this.desktop_modifiers_changed(id, event, window, cx);
                },
            ))
            .child(
                canvas(
                    move |bounds, _window, _cx| {
                        let rect = desktop_draw_rect(bounds, remote, zoom);
                        draw_sink.set(Some(rect));
                        bounds_sink.set(Some(bounds));
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
        let clipboard_tip = if desktop.protocol == Protocol::Rdp {
            tr!(
                cx,
                "RDP 剪贴板随系统自动同步（文本与文件），直接 Ctrl+C / Ctrl+V 即可"
            )
        } else {
            tr!(cx, "把本地剪贴板文本发送到远端")
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
            .when(desktop.protocol == Protocol::Rdp, |this| {
                this.child(
                    Button::new(SharedString::from(format!("rd-match-{id}")))
                        .label(tr!(cx, "匹配窗口"))
                        .ghost()
                        .compact()
                        .tooltip(tr!(cx, "让远端把桌面分辨率调整为当前窗口大小"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.match_desktop_window(id, window, cx);
                        })),
                )
            })
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
                    .tooltip(clipboard_tip)
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

    fn render_desktop_form(
        &self,
        tab_id: usize,
        desktop: &DesktopTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let is_rdp = desktop.protocol == Protocol::Rdp;
        let title = if is_rdp {
            tr!(cx, "远程桌面（RDP）")
        } else {
            tr!(cx, "远程桌面（VNC）")
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w(px(400.0))
                    .gap_3()
                    .p_4()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(rgpui::FontWeight(600.0))
                                    .child(title),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "rd-proto-vnc-{tab_id}"
                                        )))
                                        .label("VNC")
                                        .ghost()
                                        .compact()
                                        .selected(!is_rdp)
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.set_desktop_protocol(
                                                    tab_id,
                                                    Protocol::Vnc,
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        ),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "rd-proto-rdp-{tab_id}"
                                        )))
                                        .label("RDP")
                                        .ghost()
                                        .compact()
                                        .selected(is_rdp)
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.set_desktop_protocol(
                                                    tab_id,
                                                    Protocol::Rdp,
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(pal.muted).child(tr!(cx, "地址")))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div().flex_1().min_w_0().child(styled_input(
                                            Input::new(&desktop.form_host),
                                            &pal,
                                        )),
                                    )
                                    .child(
                                        div().w(px(96.0)).child(styled_input(
                                            Input::new(&desktop.form_port),
                                            &pal,
                                        )),
                                    ),
                            ),
                    )
                    .when(is_rdp, |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(pal.muted)
                                        .child(tr!(cx, "用户名")),
                                )
                                .child(styled_input(Input::new(&desktop.form_username), &pal)),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(pal.muted)
                                        .child(tr!(cx, "域（可留空）")),
                                )
                                .child(styled_input(Input::new(&desktop.form_domain), &pal)),
                        )
                    })
                    .child({
                        // 保存到哪个分组：新建默认未分组，打开已保存主机时回填原归属。
                        let mut options: Vec<SharedString> = vec![SharedString::from(UNGROUPED)];
                        options.extend(
                            self.sidebar
                                .names()
                                .map(|name| SharedString::from(name.to_string())),
                        );
                        let selected = desktop
                            .group
                            .as_ref()
                            .and_then(|group| options.iter().position(|option| option == group))
                            .or(Some(0));
                        let change_options = options.clone();
                        let entity = cx.entity();
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(pal.muted).child(tr!(cx, "分组")))
                            .child(
                                Select::new(options)
                                    .id("desktop-group")
                                    .selected(selected)
                                    .w_full()
                                    .on_change(move |index, _name, _window, cx| {
                                        let group = if index == 0 {
                                            None
                                        } else {
                                            change_options.get(index).cloned()
                                        };
                                        entity.update(cx, |this, cx| {
                                            if let Some(desktop) = this
                                                .tabs
                                                .iter_mut()
                                                .find(|tab| tab.id == tab_id)
                                                .and_then(|tab| tab.desktop.as_mut())
                                            {
                                                desktop.group = group;
                                                cx.notify();
                                            }
                                        });
                                    }),
                            )
                    })
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
                    .when(self.vault.is_some(), |this| {
                        // 与 SSH 一致：勾选后才把密码加密入库，默认不落盘。
                        // 点击交给 Checkbox 的 on_change（label 也能点），容器只负责排版。
                        let entity = cx.entity();
                        this.child(
                            Checkbox::new(SharedString::from(format!("rd-save-secret-{tab_id}")))
                                .checked(desktop.save_secret)
                                .label(tr!(cx, "保存密码"))
                                .on_change(move |value, _window, cx| {
                                    entity.update(cx, |this, cx| {
                                        if let Some(desktop) = this
                                            .tabs
                                            .iter_mut()
                                            .find(|tab| tab.id == tab_id)
                                            .and_then(|tab| tab.desktop.as_mut())
                                        {
                                            desktop.save_secret = value;
                                            cx.notify();
                                        }
                                    });
                                }),
                        )
                    })
                    .child(h_flex().items_center().gap_1().child({
                        // 密码去向取决于凭据库是否可用与是否勾选保存。
                        let hint = if self.vault.is_some() {
                            tr!(
                                cx,
                                "勾选「保存密码」后，密码会加密存入凭据库；不勾选则只在本次使用"
                            )
                        } else {
                            tr!(cx, "保存主机只记地址与用户名；密码只在本次连接的内存里使用")
                        };
                        div().text_xs().text_color(pal.muted).child(hint)
                    }))
                    .when_some(desktop.form_error.clone(), |this, error| {
                        this.child(
                            div()
                                .text_xs()
                                .font_weight(rgpui::FontWeight(500.0))
                                .text_color(pal.danger)
                                .child(error),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new(SharedString::from(format!("rd-cancel-{tab_id}")))
                                    .label(tr!(cx, "取消"))
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.cancel_desktop_form(tab_id, cx);
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("rd-save-{tab_id}")))
                                    .label(if desktop.saved_host.is_some() {
                                        tr!(cx, "保存修改")
                                    } else {
                                        tr!(cx, "保存")
                                    })
                                    .secondary()
                                    .outline()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.save_desktop_host(tab_id, cx);
                                    })),
                            )
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

    /// 直连标签的密码卡：只问密码，地址 / 账号来自已保存主机，不再展示完整表单。
    fn render_desktop_password(
        &self,
        tab_id: usize,
        desktop: &DesktopTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let title = if desktop.protocol == Protocol::Rdp {
            tr!(cx, "远程桌面（RDP）")
        } else {
            tr!(cx, "远程桌面（VNC）")
        };
        let account = if desktop.protocol == Protocol::Rdp && !desktop.username.is_empty() {
            if desktop.domain.is_empty() {
                desktop.username.clone()
            } else {
                format!("{}\\{}", desktop.domain, desktop.username)
            }
        } else {
            String::new()
        };
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
                            .child(title),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(host_port(&desktop.host, desktop.port))
                            .when(!account.is_empty(), |this| {
                                this.child(div().child(format!("· {account}")))
                            }),
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
                    .child({
                        // 直连标签的 save_secret 恒为 false（表单里勾选的
                        // 「保存密码」只在主机编辑表单生效）：这里如实说明，
                        // 不承诺「会保存到凭据库」。
                        let hint = if self.vault.is_some() {
                            tr!(
                                cx,
                                "密码只在本次连接的内存里复用；想让它记住密码，编辑该主机并勾选「保存密码」；留空表示不带密码"
                            )
                        } else {
                            tr!(cx, "密码只在本次连接的内存里使用；留空表示不带密码")
                        };
                        div().text_xs().text_color(pal.muted).child(hint)
                    })
                    .when_some(desktop.form_error.clone(), |this, error| {
                        this.child(
                            div()
                                .text_xs()
                                .font_weight(rgpui::FontWeight(500.0))
                                .text_color(pal.danger)
                                .child(error),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new(SharedString::from(format!("rd-edit-form-{tab_id}")))
                                    .label(tr!(cx, "修改参数"))
                                    .ghost()
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_desktop_form(tab_id, cx);
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("rd-cancel-{tab_id}")))
                                    .label(tr!(cx, "取消"))
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.cancel_desktop_form(tab_id, cx);
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("rd-connect-{tab_id}")))
                                    .label(tr!(cx, "连接"))
                                    .primary()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.submit_desktop_password(tab_id, cx);
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
                                        this.status = tr!(cx, "错误已复制到剪贴板");
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
/// 端口 0 一律拒绝（连接只会报晦涩错误）；`host:`（尾冒号没数字）剥掉
/// 尾冒号按纯主机名处理，不把整串丢给 DNS。
fn normalize_address(raw: &str, port_text: &str) -> (String, Option<u16>) {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once("]:")
        && let Ok(port) = tail.trim().parse::<u16>()
        && port != 0
    {
        // 连接用裸 IPv6 字面量，展示时再由 `host_port` 补回方括号。
        return (host.to_string(), Some(port));
    }
    if let Some((host, tail)) = raw.rsplit_once(':')
        && !host.contains(':')
        && !host.is_empty()
        && let Ok(port) = tail.trim().parse::<u16>()
        && port != 0
    {
        return (host.to_string(), Some(port));
    }
    if raw.ends_with(':') {
        // `host:`：尾冒号解析不出端口，剥掉避免整串被当主机名。
        return normalize_address(raw.trim_end_matches(':'), port_text);
    }
    let port = port_text.parse::<u16>().ok().filter(|port| *port != 0);
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

/// 认证失败判定：RDP 看凭据类报错，VNC 看协议层「认证失败」文案。
/// 「服务器要求 VNC 密码」（空密码连强制认证的服务器）也归认证失败：
/// 要退回密码表单，而不是停在失败卡片上无限重试同样的空密码。
fn desktop_auth_failure(protocol: Protocol, message: &str) -> bool {
    match protocol {
        Protocol::Rdp => looks_like_auth_failure(message),
        Protocol::Vnc => message.contains("认证失败") || message.contains("要求 VNC 密码"),
    }
}

/// 桌面密码的进程内缓存键：账号 + 地址 + 端口，换账号不会误用旧密码。
fn desktop_cache_key(host: &str, port: u16, username: &str, domain: &str) -> String {
    let user = if username.is_empty() {
        domain.to_string()
    } else if domain.is_empty() {
        username.to_string()
    } else {
        format!("{domain}\\{username}")
    };
    if user.is_empty() {
        format!("{host}:{port}")
    } else {
        format!("{user}@{host}:{port}")
    }
}

/// 把远程桌面密码加密写入凭据库（与 SSH 密码同一套）。返回是否已提交入库。
///
/// 只对已保存主机（id 非 0）且库已解锁时生效；写库是异步的，不阻塞界面线程。
fn store_desktop_secret(
    vault: Option<Arc<Vault>>,
    saved_host: Option<i64>,
    password: &str,
    cx: &mut Context<RuTermApp>,
) -> bool {
    let (Some(vault), Some(host_id)) = (vault, saved_host.filter(|id| *id != 0)) else {
        return false;
    };
    if password.is_empty() {
        return false;
    }
    let secret = password.to_string();
    cx.spawn(async move |_, _| {
        if let Err(error) = vault.set_password(host_id, Some(&secret)).await {
            log::warn!("保存远程桌面凭据失败: {error}");
        }
    })
    .detach();
    true
}

/// 保存主机后写密码：先 upsert 确保行存在（新建主机时 id 是刚分配的），再加密写库；
/// 两步放在同一任务里，避免与 `persist_host` 的并发写入抢先后导致密码落在不存在的行上。
fn store_desktop_secret_after_save(
    vault: Option<Arc<Vault>>,
    host: HostConfig,
    password: &str,
    cx: &mut Context<RuTermApp>,
) -> bool {
    let Some(vault) = vault else {
        return false;
    };
    if host.id == 0 || password.is_empty() {
        return false;
    }
    let secret = password.to_string();
    cx.spawn(async move |_, _| {
        if let Err(error) = vault.upsert(&host).await {
            log::warn!("保存远程桌面主机失败: {error}");
            return;
        }
        if let Err(error) = vault.set_password(host.id, Some(&secret)).await {
            log::warn!("保存远程桌面凭据失败: {error}");
        }
    })
    .detach();
    true
}

/// 认证失败后清掉库里存的旧密码，否则重启后直连仍会拿失效密码重试。
fn clear_desktop_secret(
    vault: Option<Arc<Vault>>,
    saved_host: Option<i64>,
    cx: &mut Context<RuTermApp>,
) {
    let (Some(vault), Some(host_id)) = (vault, saved_host.filter(|id| *id != 0)) else {
        return;
    };
    cx.spawn(async move |_, _| {
        if let Err(error) = vault.set_password(host_id, None).await {
            log::warn!("清除远程桌面凭据失败: {error}");
        }
    })
    .detach();
}

/// RDP 失败信息是否属于「凭据不对」：这类错误回表单让用户改密码，
/// 比停在失败卡片上更接近系统远程桌面的体验。
fn looks_like_auth_failure(message: &str) -> bool {
    let text = message.to_lowercase();
    // 只认足够具体的凭据错误词。像 "password" / "authenticat" / "凭据" 这类
    // 泛化子串会把密码过期、账号锁定甚至含 credentials 的网络错误都误判成
    // 认证失败，进而清掉内存缓存和加密库里的密码——宁可漏判也不误清。
    [
        "credssp",
        "access denied",
        "access_denied",
        "logon failure",
        "logon_failure",
        "status_logon",
        "0xc000006d",
        "0xc000006a",
        "登录失败",
        "用户名或密码",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// BGRA 快照 → rgpui 纹理帧（`RenderImage` 的数据顺序就是 BGRA）。
fn snapshot_to_image(mut snapshot: Snapshot) -> Arc<RenderImage> {
    let width = u32::from(snapshot.width);
    let height = u32::from(snapshot.height);
    // 快照取走后就没人再用这块内存：直接移交所有权，省掉 1080p 下每帧 ~8MB 的拷贝。
    let bgra = std::mem::take(&mut snapshot.bgra);
    let buffer = image::RgbaImage::from_raw(width, height, bgra)
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
    fn desktop_password_cache_key_keys_by_account() {
        assert_eq!(
            desktop_cache_key("10.0.0.5", 3389, "alice", "CORP"),
            "CORP\\alice@10.0.0.5:3389"
        );
        assert_eq!(desktop_cache_key("10.0.0.5", 5900, "", ""), "10.0.0.5:5900");
        assert_eq!(
            desktop_cache_key("10.0.0.5", 3389, "", "CORP"),
            "CORP@10.0.0.5:3389"
        );
    }

    #[test]
    fn auth_failures_are_protocol_aware() {
        assert!(desktop_auth_failure(
            Protocol::Vnc,
            "认证失败：too many attempts"
        ));
        assert!(!desktop_auth_failure(Protocol::Vnc, "connection reset"));
        assert!(desktop_auth_failure(
            Protocol::Rdp,
            "CredSSP: STATUS_LOGON_FAILURE"
        ));
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
