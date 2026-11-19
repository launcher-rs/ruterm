//! //! 建连流程：点击主机 → 取密（缓存 / 加密库 / 弹窗）→ SSH / Telnet 会话任务拉起。

use std::sync::Arc;
use std::time::{Duration, Instant};

use rgpui::input_ui::{Input, InputContentType, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, Entity, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Window, div, h_flex, px, v_flex,
};
use rgpui_term::{Event as TerminalEvent, TerminalBuilder, TerminalView};

use crate::config::SshTarget;
use crate::sessions::{AuthMethod, HostConfig, ProtocolKind};

use super::*;

/// 弹窗索取的秘密类型：登录密码或加密私钥的口令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PromptKind {
    Password,
    Passphrase,
}

/// 待输入密码/口令后连接的会话（点击需要秘密的主机时进入）。
pub(super) struct PendingConnect {
    pub(super) host: HostConfig,
    pub(super) password: Entity<InputState>,
    /// 是否勾选"保存"（仅加密库可用时生效，默认否＝本次内存持有）。
    pub(super) save_password: bool,
    /// 弹窗类型（决定文案与连接参数落点）。
    pub(super) kind: PromptKind,
}

impl RuTermApp {
    pub(super) fn ssh_ready(&self) -> bool {
        self.auth_gate.ready() && self.vault.is_some() && !self.vault_maintenance
    }

    pub(super) fn require_ssh_ready(&mut self, cx: &mut Context<Self>) -> bool {
        if self.ssh_ready() {
            return true;
        }
        self.status = "凭据库未就绪或已锁定；解锁后才能进行 SSH 认证".into();
        cx.notify();
        false
    }

    /// 点击主机账号：密码/私钥口令按 内存缓存 → 加密库 → 弹窗 的顺序取得；Agent 直接连接。
    /// 凭 id 定位：列表可能在此前被后台 attach_vault 整表替换，下标会指向别的行。
    pub(super) fn click_host(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter().find(|host| host.id == id).cloned() else {
            return;
        };
        // 桌面主机（RDP / VNC）：开桌面标签直接建连，缺密码时只问密码（不入库）。
        if host.protocol.is_desktop() {
            self.open_desktop_for_saved(&host, window, cx);
            return;
        }
        // Telnet 是明文 TCP，没有认证材料可问，直接建连。
        if host.protocol == ProtocolKind::Telnet {
            self.connect_telnet(&host, window, cx);
            return;
        }
        if !self.require_ssh_ready(cx) {
            return;
        }
        match host.auth {
            AuthMethod::Agent => {
                let target = match self.host_target(&host, None) {
                    Ok(target) => target,
                    Err(error) => {
                        self.status = SharedString::from(error);
                        cx.notify();
                        return;
                    }
                };
                let title = Self::host_title(&host);
                // 带上 host：Agent 主机也能进最近连接，且重连时按最新主机配置重建。
                self.connect(title, target, None, Some(host), false, window, cx);
            }
            AuthMethod::Password | AuthMethod::IdentityFile => {
                self.click_secret_host(host, window, cx);
            }
        }
    }

    /// 密码/私钥主机的取密顺序：缓存 → 加密库 →（密码）弹窗 /（私钥）免口令直连探测。
    pub(super) fn click_secret_host(
        &mut self,
        host: HostConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require_ssh_ready(cx) {
            return;
        }
        let key = host.addr();
        if let Some(secret) = self.password_cache.get(&key).cloned() {
            self.connect_with_secret(host, secret, key, false, window, cx);
            return;
        }
        if self.pending.is_some() {
            return;
        }
        // 加密库可用：异步查已存秘密，命中直连，未命中回弹窗 / 免口令试连（口令信号由会话回抛）。
        if let (Some(vault), true) = (self.vault.clone(), host.id != 0) {
            let generation = self.auth_gate.generation();
            let entity = cx.entity();
            let entity_id = entity.entity_id();
            cx.spawn(async move |_, cx| {
                let saved = vault.password_of(host.id).await.ok().flatten();
                // 跳板机的凭据也从库里取：目标主机点击时才需要输入密码，
                // 跳板机不能每次都追问一遍（它只是连接路径的一部分）。
                let saved_jump = match host.proxy_jump {
                    Some(id) if id != host.id => vault.password_of(id).await.ok().flatten(),
                    _ => None,
                };
                let _ = cx.with_window(entity_id, move |window, cx| {
                    entity.update(cx, move |this, cx| {
                        if !this.auth_gate.accepts(generation)
                            || !this.ssh_ready()
                            || this.pending.is_some()
                        {
                            return;
                        }
                        if let Some(secret) = saved_jump
                            && let Some(jump) = this
                                .hosts
                                .iter()
                                .find(|item| Some(item.id) == host.proxy_jump)
                        {
                            let key = jump.addr();
                            this.password_cache.insert(key, secret);
                        }
                        match (saved, host.auth) {
                            (Some(secret), _) => {
                                let key = host.addr();
                                this.connect_with_secret(host, secret, key, true, window, cx);
                            }
                            (None, AuthMethod::Password) => {
                                this.prompt_secret(host, PromptKind::Password, window, cx);
                            }
                            // 私钥：先不带口令直连；密钥确有口令时回传 PassphraseRequired 再弹窗。
                            (None, _) => this.connect_without_secret(host, window, cx),
                        }
                    });
                });
            })
            .detach();
            return;
        }
        match host.auth {
            AuthMethod::Password => self.prompt_secret(host, PromptKind::Password, window, cx),
            _ => self.connect_without_secret(host, window, cx),
        }
    }

    /// 由主机配置 + 本次秘密构造连接目标，并带上加密库里的内嵌私钥。
    ///
    /// 标记为内嵌却取不到私钥时返回 Err：宁可明确报错，也不静默退回 SSH Agent
    /// 让用户以为在用另一把钥匙。
    pub(super) fn host_target(
        &self,
        host: &HostConfig,
        password: Option<String>,
    ) -> Result<SshTarget, String> {
        if !self.ssh_ready() {
            return Err("凭据库未就绪，拒绝读取认证材料".into());
        }
        let key_data = if host.auth == AuthMethod::IdentityFile && host.key_inline {
            Some(self.inline_keys.get(&host.id).cloned().ok_or_else(|| {
                "内嵌私钥不可用（加密库未解锁或数据损坏），请编辑该主机重新导入".to_string()
            })?)
        } else {
            None
        };
        let jump = self.jump_target(host)?;
        Ok(SshTarget::from_host(host, password)
            .with_key_data(key_data)
            .with_jump(jump))
    }

    /// 把主机表单里选的跳板机构造成完整的连接目标（含它自己的认证材料）。
    ///
    /// 跳板机必须是另一台已保存主机；密码 / 口令只从本次运行的内存缓存里取
    /// （先单独连一次该跳板机并勾选保存即可），绝不为了跳板机再弹一次窗
    /// 或落明文。取不到时给出可操作的提示，而不是让连接失败得莫名其妙。
    pub(super) fn jump_target(&self, host: &HostConfig) -> Result<Option<SshTarget>, String> {
        let Some(jump_id) = host.proxy_jump else {
            return Ok(None);
        };
        let jump = self
            .hosts
            .iter()
            .find(|item| item.id == jump_id)
            .ok_or_else(|| "所选跳板机已不存在，请编辑该主机重新选择".to_string())?;
        if jump.id == host.id {
            return Err("不能把主机自己设为跳板机".to_string());
        }
        if jump.proxy_jump.is_some() {
            return Err("跳板机不能再挂跳板机，请先解除它的跳板机设置".to_string());
        }
        if jump.protocol != ProtocolKind::Ssh {
            return Err("所选跳板机不是 SSH 主机，不能作为跳板".to_string());
        }
        let secret = self.password_cache.get(&jump.addr()).cloned();
        let password = if jump.auth == AuthMethod::Password {
            Some(secret.clone().ok_or_else(|| {
                format!(
                    "跳板机 {} 需要密码：请先单独连接它并勾选「保存密码」，或改用私钥 / Agent",
                    jump.addr()
                )
            })?)
        } else {
            None
        };
        let key_data = if jump.auth == AuthMethod::IdentityFile && jump.key_inline {
            Some(self.inline_keys.get(&jump.id).cloned().ok_or_else(|| {
                format!(
                    "跳板机 {} 的内嵌私钥不可用（加密库未解锁或数据损坏）",
                    jump.addr()
                )
            })?)
        } else {
            None
        };
        // 私钥口令走 `passphrase`；密码认证的 secret 已在上面作为 password 传了。
        let passphrase = if jump.auth == AuthMethod::Password {
            None
        } else {
            secret
        };
        Ok(Some(
            SshTarget::from_host(jump, password)
                .with_key_data(key_data)
                .with_passphrase(passphrase),
        ))
    }

    /// 用取到的秘密（缓存或加密库）建立连接目标并连接。
    pub(super) fn connect_with_secret(
        &mut self,
        host: HostConfig,
        secret: String,
        key: String,
        from_vault: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match host.auth {
            AuthMethod::Password => self.host_target(&host, Some(secret)),
            _ => self
                .host_target(&host, None)
                .map(|target| target.with_passphrase(Some(secret))),
        };
        let target = match target {
            Ok(target) => target,
            Err(error) => {
                self.status = SharedString::from(error);
                cx.notify();
                return;
            }
        };
        let title = Self::host_title(&host);
        self.connect(title, target, Some(key), Some(host), from_vault, window, cx);
    }

    /// 不带秘密直连（私钥主机的口令探测路径）。
    pub(super) fn connect_without_secret(
        &mut self,
        host: HostConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match self.host_target(&host, None) {
            Ok(target) => target,
            Err(error) => {
                self.status = SharedString::from(error);
                cx.notify();
                return;
            }
        };
        let title = Self::host_title(&host);
        self.connect(title, target, None, Some(host), false, window, cx);
    }

    /// 弹出密码/私钥口令输入面板（默认不保存；加密库可用时提供"保存"勾选）。
    pub(super) fn prompt_secret(
        &mut self,
        host: HostConfig,
        kind: PromptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if host.protocol == ProtocolKind::Ssh && !self.require_ssh_ready(cx) {
            return;
        }
        let placeholder = match kind {
            PromptKind::Password => "输入密码后点击连接",
            PromptKind::Passphrase => "输入私钥口令后点击连接",
        };
        let password = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        password.update(cx, |state, cx| state.focus(window, cx));
        self.pending = Some(PendingConnect {
            host,
            password,
            save_password: false,
            kind,
        });
        cx.notify();
    }

    /// 取消待输入密码的连接。
    pub(super) fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        cx.notify();
    }

    /// 用输入的秘密完成待连接：内存缓存必写；勾选保存且加密库可用时再持久化入库。
    pub(super) fn confirm_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let secret = pending.password.read(cx).value().to_string();
        let host = pending.host;
        let save = pending.save_password;
        let kind = pending.kind;
        let key = host.addr();
        self.password_cache.insert(key.clone(), secret.clone());
        if save && let (Some(vault), true) = (self.vault.clone(), host.id != 0) {
            let stored = secret.clone();
            let id = host.id;
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.set_password(id, Some(&stored)).await {
                    log::warn!("保存凭据失败: {error}");
                }
            })
            .detach();
        }
        let target = match kind {
            PromptKind::Password => self.host_target(&host, Some(secret)),
            PromptKind::Passphrase => self
                .host_target(&host, None)
                .map(|target| target.with_passphrase(Some(secret))),
        };
        let target = match target {
            Ok(target) => target,
            Err(error) => {
                self.status = SharedString::from(error);
                cx.notify();
                return;
            }
        };
        self.connect(
            Self::host_title(&host),
            target,
            Some(key),
            Some(host),
            false,
            window,
            cx,
        );
    }

    /// 建立外接终端并启动 SSH 会话，追加为新标签。
    ///
    /// shell 开启前的失败（连不上 / 认证被拒 / 私钥需口令）**保留标签页**，
    /// 错误文本留在终端里可读，不再一闪而过；需要口令时自动弹口令窗。
    /// 被拒绝的秘密会清掉内存缓存；若来自加密库，库内条目也一并清除，
    /// 下次点击重新询问，而不是拿坏凭据反复直连。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn connect(
        &mut self,
        title: String,
        target: SshTarget,
        cache_key: Option<String>,
        host: Option<HostConfig>,
        secret_from_vault: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        // 新会话成为活动标签，分屏里没有它，回到单屏。
        self.view_split = None;
        self.active_tab = Some(id);
        self.start_ssh(
            id,
            title,
            target,
            cache_key,
            host,
            secret_from_vault,
            0,
            false,
            window,
            cx,
        );
    }

    /// 在指定标签槽位启动 SSH 会话：`reconnect` 为真时原位替换该标签的终端，
    /// 否则追加为新标签。`retry_attempt` 为自动重连的连续次数（手动连接为 0）。
    /// 会话结束（远端 exit / 断线）由终端 `CloseTerminal` 事件收尾——标签不摘除，
    /// 改为展示断开横幅。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_ssh(
        &mut self,
        id: usize,
        title: String,
        target: SshTarget,
        cache_key: Option<String>,
        host: Option<HostConfig>,
        secret_from_vault: bool,
        retry_attempt: u32,
        reconnect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(mut builder) = TerminalBuilder::new_external(None) else {
            log::error!("无法创建外接终端后端");
            return;
        };
        let Some(handles) = builder.take_external_handles() else {
            log::error!("new_external 之后缺失外接句柄");
            return;
        };
        let terminal = cx.new(|cx| builder.subscribe(cx));
        // 同步输入的观察点：终端所有输入都会经过中间件，程序化写回的会被标记为
        // Programmatic 从而不再反弹。
        let mirror_tx = self.mirror_tx.clone();
        terminal.update(cx, |terminal, _| {
            terminal.add_middleware(Arc::new(NormalizePastedLineEndings));
            terminal.add_middleware(Arc::new(MirrorInput {
                tab_id: id,
                tx: mirror_tx,
            }));
        });
        let style = self.terminal_text_style(cx);
        let terminal_view =
            cx.new(|cx| TerminalView::new_with_style(terminal.clone(), style, window, cx));
        // 状态栏展示用的连接位置与认证方式，须在 target 被 crate::session::start 消费前取出。
        let addr = format!("{}@{}:{}", target.user, target.host, target.port);
        let auth = if target.password.is_some() {
            AuthMethod::Password
        } else if target.key_path.is_some() || target.key_data.is_some() {
            AuthMethod::IdentityFile
        } else {
            AuthMethod::Agent
        };
        let resume = Resume::Ssh {
            target: target.clone(),
            host: host.clone(),
            secret_from_vault,
        };
        let crate::session::SshSession {
            join,
            mut remote_info,
            mut cwd_events,
            started,
            ready,
            disconnect,
            commands,
        } = crate::session::start(
            target,
            handles,
            self.vault.clone(),
            Duration::from_secs(self.session_prefs.connect_timeout),
            cx,
        );
        // 会话结束（远端 exit / 断线 / 连接失败）→ 保留标签并展示断开横幅。
        // 捕获弱引用：订阅 detach 后活到终端实体销毁为止，强引用会把
        // 旧 TerminalView（连同整份回滚网格）钉在闭包里永不释放。
        let close_view = terminal_view.downgrade();
        cx.subscribe(&terminal, move |this, _, event: &TerminalEvent, cx| {
            // 只认当前标签的终端：重连后旧终端若迟来 CloseTerminal，不能把新会话标成已断开。
            let Some(close_view) = close_view.upgrade() else {
                return;
            };
            if matches!(event, TerminalEvent::CloseTerminal) && this.tab_is_current(id, &close_view)
            {
                this.end_session_tab(id, cx);
            }
        })
        .detach();
        // 查找是排队执行的：终端 sync 完才定状态，收到结果后重绘查找条。
        self.watch_terminal(id, &terminal, cx);
        self.status = SharedString::from(format!("正在连接 {addr}…"));
        // 原标签重连沿用原序号；新标签补最小空号，已有标签的名字不受影响。
        let title_ordinal = if reconnect {
            self.tabs
                .iter()
                .find(|tab| tab.id == id)
                .map(|tab| tab.title_ordinal)
                .unwrap_or(1)
        } else {
            self.next_title_ordinal(&title, None)
        };
        let tab = SessionTab {
            id,
            title: title.clone().into(),
            base_title: title.into(),
            custom_title: None,
            title_ordinal,
            follow_osc_title: false,
            addr: addr.clone().into(),
            auth: Some(auth),
            remote_info: None,
            platform: None,
            terminal_view: Some(terminal_view.clone()),
            desktop: None,
            ended: false,
            failed: false,
            last_error: None,
            resume: Some(resume),
            disconnect: Some(disconnect),
            commands: Some(commands),
            forwards: Vec::new(),
            files: FileBrowser::default(),
            terminal_cwd: None,
            zmodem: None,
            zmodem_progress: None,
            auto_retry_attempt: retry_attempt,
            auto_retry_pending: false,
            connected_at: None,
        };
        match self.tabs.iter_mut().find(|tab| tab.id == id) {
            Some(existing) if reconnect => *existing = tab,
            Some(_) => log::warn!("标签 id {id} 已存在，忽略重复落标签"),
            None => self.tabs.push(tab),
        }
        // 新标签/重连后把焦点交回终端，键入住处即可继续敲命令。
        self.activate_tab(id, window, cx);
        let ready_view = terminal_view.clone();
        let ready_addr = addr.clone();
        let recent_host_id = host.as_ref().map(|host| host.id).filter(|id| *id != 0);
        cx.spawn(async move |this, cx| {
            if ready.await.is_err() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                let current = this
                    .tabs
                    .iter_mut()
                    .find(|tab| tab.id == id && tab.terminal_view.as_ref() == Some(&ready_view));
                if let Some(tab) = current {
                    tab.connected_at = Some(Instant::now());
                    tab.auto_retry_pending = false;
                } else {
                    return;
                }
                if let Some(host_id) = recent_host_id {
                    this.record_recent_host(host_id, cx);
                }
                // 连上后状态栏不该一直停在"正在连接"；但用户此时可能已经看到别的提示，只在原文案未变时替换。
                let connecting = format!("正在连接 {ready_addr}…");
                if this.status == connecting {
                    this.status = SharedString::from(format!("已连接 {ready_addr}"));
                }
                cx.notify();
                this.auto_start_forwards(id, cx);
            });
        })
        .detach();
        let vault = self.vault.clone();
        let current_view = terminal_view.clone();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let shell_started = started.load(std::sync::atomic::Ordering::SeqCst);
            let Some(entity) = this.upgrade() else {
                return;
            };
            let entity_id = entity.entity_id();
            let _ = cx.with_window(entity_id, move |window, cx| {
                entity.update(cx, move |this, cx| {
                    if !this.tab_is_current(id, &current_view) {
                        return;
                    }
                    // 正常结束（远端 exit / 断线）由终端的 CloseTerminal 收尾，这里无事可做。
                    let Err(error) = result else {
                        return;
                    };
                    // 加密私钥缺口令：回收死标签，自动弹口令窗。
                    if error
                        .downcast_ref::<crate::session::PassphraseRequired>()
                        .is_some()
                    {
                        this.remove_tab(id, cx);
                        if let Some(host) = host {
                            this.prompt_secret(host, PromptKind::Passphrase, window, cx);
                        } else {
                            this.status =
                                "私钥已加密，需要口令（快速连接请改用 -i 对应明文密钥）".into();
                            cx.notify();
                        }
                        return;
                    }
                    let rejected = error
                        .downcast_ref::<crate::session::SecretRejected>()
                        .is_some();
                    if rejected {
                        if let Some(key) = &cache_key {
                            this.password_cache.remove(key);
                        }
                        // 凭据已被拒绝：带着同一份秘密重连只会再失败一次，
                        // 关掉重连入口，提示用户回列表重新点击以输入新凭据。
                        if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == id) {
                            tab.resume = None;
                        }
                        if secret_from_vault
                            && let (Some(vault), Some(h)) = (vault.clone(), host.as_ref())
                            && h.id != 0
                        {
                            let row = h.id;
                            cx.spawn(async move |_, _| {
                                if let Err(error) = vault.set_password(row, None).await {
                                    log::warn!("清除失效凭据失败: {error}");
                                }
                            })
                            .detach();
                            this.status = SharedString::from(
                                "已保存的凭据被服务器拒绝，已清除；再次点击将重新输入",
                            );
                        }
                    }
                    let message = if shell_started {
                        format!(
                            "连接中断：{addr}（{}）",
                            crate::session::error_summary(&error)
                        )
                    } else {
                        // 建连阶段失败同样带上原因：横幅的「复制错误」要有内容可复制。
                        format!(
                            "连接失败：{addr}（{}）",
                            crate::session::error_summary(&error)
                        )
                    };
                    this.fail_session_tab(id, message.into(), shell_started, cx);
                });
            });
        })
        .detach();
        // 远程基本信息按固定间隔持续回传：负载/内存/磁盘会随会话变化，只写第一次会一直停在连接时的值。
        let info_view = terminal_view.clone();
        cx.spawn(async move |this, cx| {
            while let Some(info) = remote_info.recv().await {
                let _ =
                    this.update(cx, |this, cx| {
                        if let Some(tab) = this.tabs.iter_mut().find(|tab| {
                            tab.id == id && tab.terminal_view.as_ref() == Some(&info_view)
                        }) {
                            tab.remote_info = Some(info);
                            cx.notify();
                        }
                    });
            }
        })
        .detach();
        // 终端工作目录确认流：仅在 SFTP 校验"确为目录"后到达。
        // 始终更新 tab.terminal_cwd；若面板跟随开启且当前标签就是本会话，则立即跳转。
        let cwd_view = terminal_view.clone();
        cx.spawn(async move |this, cx| {
            while let Some(path) = cwd_events.recv().await {
                log::debug!("[cwd_events] 收到确认路径: {}", path);
                let _ = this.update(cx, |this, cx| {
                    let Some(pos) = this.tabs.iter().position(|tab| {
                        tab.id == id && tab.terminal_view.as_ref() == Some(&cwd_view)
                    }) else {
                        log::debug!("[cwd_events] 未找到对应标签");
                        return;
                    };
                    let sidebar_visible = this.sidebar_visible(cx);
                    let tab = &mut this.tabs[pos];
                    let changed = tab.terminal_cwd.as_deref() != Some(path.as_str());
                    tab.terminal_cwd = Some(path.clone());
                    let should_follow = tab.files.follow
                        && sidebar_visible
                        && this.sidebar_page == SidebarPage::Files
                        && this.active_tab == Some(id)
                        && tab.files.path.as_deref() != Some(path.as_str());
                    log::debug!(
                        "[cwd_events] follow={}, panel={}, active={}, path_diff={}",
                        tab.files.follow,
                        sidebar_visible,
                        this.active_tab == Some(id),
                        tab.files.path.as_deref() != Some(path.as_str())
                    );
                    if should_follow {
                        log::debug!("[cwd_events] 触发跟随，加载: {}", path);
                        this.load_files(id, Some(path), cx);
                    } else if changed {
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// 建立外接终端并启动 Telnet 会话，追加为新标签。
    pub(super) fn connect_telnet(
        &mut self,
        host: &HostConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        self.view_split = None;
        self.active_tab = Some(id);
        let title = if host.name.trim().is_empty() {
            format!("{}:{}", host.host, host.port)
        } else {
            host.name.clone()
        };
        self.start_telnet(
            id,
            title,
            host.host.clone(),
            host.port,
            Some(host.clone()),
            0,
            false,
            window,
            cx,
        );
    }

    /// 在指定标签槽位启动 Telnet 会话（与 [`Self::start_ssh`] 同形的落标签流程）：
    /// 会话结束保留终端内容与断开横幅，可原标签重连。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_telnet(
        &mut self,
        id: usize,
        title: String,
        host: String,
        port: u16,
        source: Option<HostConfig>,
        retry_attempt: u32,
        reconnect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(mut builder) = TerminalBuilder::new_external(None) else {
            log::error!("无法创建外接终端后端");
            return;
        };
        let Some(handles) = builder.take_external_handles() else {
            log::error!("new_external 之后缺失外接句柄");
            return;
        };
        let terminal = cx.new(|cx| builder.subscribe(cx));
        // 同步输入同样覆盖 Telnet 会话。
        let mirror_tx = self.mirror_tx.clone();
        terminal.update(cx, |terminal, _| {
            terminal.add_middleware(Arc::new(NormalizePastedLineEndings));
            terminal.add_middleware(Arc::new(MirrorInput {
                tab_id: id,
                tx: mirror_tx,
            }));
        });
        let style = self.terminal_text_style(cx);
        let terminal_view =
            cx.new(|cx| TerminalView::new_with_style(terminal.clone(), style, window, cx));
        let addr = format!("{host}:{port}");
        let resume = Resume::Telnet {
            host: host.clone(),
            port,
            source: source.clone(),
        };
        let crate::telnet::TelnetSession {
            join,
            started,
            ready,
            disconnect,
        } = crate::telnet::start(
            host,
            port,
            handles,
            Duration::from_secs(self.session_prefs.connect_timeout),
            cx,
        );
        // 远端关闭 / 断线 → 保留标签并展示断开横幅。捕获弱引用，理由同 SSH 侧。
        let close_view = terminal_view.downgrade();
        cx.subscribe(&terminal, move |this, _, event: &TerminalEvent, cx| {
            let Some(close_view) = close_view.upgrade() else {
                return;
            };
            if matches!(event, TerminalEvent::CloseTerminal) && this.tab_is_current(id, &close_view)
            {
                this.end_session_tab(id, cx);
            }
        })
        .detach();
        self.watch_terminal(id, &terminal, cx);
        self.status = SharedString::from(format!("正在连接 Telnet {addr}…"));
        let title_ordinal = if reconnect {
            self.tabs
                .iter()
                .find(|tab| tab.id == id)
                .map(|tab| tab.title_ordinal)
                .unwrap_or(1)
        } else {
            self.next_title_ordinal(&title, None)
        };
        let tab = SessionTab {
            id,
            title: title.clone().into(),
            base_title: title.into(),
            custom_title: None,
            title_ordinal,
            follow_osc_title: false,
            addr: addr.clone().into(),
            auth: None,
            remote_info: None,
            platform: None,
            terminal_view: Some(terminal_view.clone()),
            desktop: None,
            ended: false,
            failed: false,
            last_error: None,
            resume: Some(resume),
            disconnect: Some(disconnect),
            commands: None,
            forwards: Vec::new(),
            files: FileBrowser::default(),
            terminal_cwd: None,
            zmodem: None,
            zmodem_progress: None,
            auto_retry_attempt: retry_attempt,
            auto_retry_pending: false,
            connected_at: None,
        };
        match self.tabs.iter_mut().find(|tab| tab.id == id) {
            Some(existing) if reconnect => *existing = tab,
            Some(_) => log::warn!("标签 id {id} 已存在，忽略重复落标签"),
            None => self.tabs.push(tab),
        }
        self.activate_tab(id, window, cx);
        let ready_view = terminal_view.clone();
        let ready_addr = addr.clone();
        let recent_host_id = source.as_ref().map(|host| host.id).filter(|id| *id != 0);
        cx.spawn(async move |this, cx| {
            if ready.await.is_err() {
                return;
            }
            let _ =
                this.update(cx, |this, cx| {
                    let Some(tab) = this.tabs.iter_mut().find(|tab| {
                        tab.id == id && tab.terminal_view.as_ref() == Some(&ready_view)
                    }) else {
                        return;
                    };
                    tab.connected_at = Some(Instant::now());
                    tab.auto_retry_pending = false;
                    if let Some(host_id) = recent_host_id {
                        this.record_recent_host(host_id, cx);
                    }
                    let connecting = format!("正在连接 Telnet {ready_addr}…");
                    if this.status == connecting {
                        this.status = SharedString::from(format!("已连接 Telnet {ready_addr}"));
                    }
                    cx.notify();
                });
        })
        .detach();
        let current_view = terminal_view.clone();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let started = started.load(std::sync::atomic::Ordering::SeqCst);
            let Some(entity) = this.upgrade() else {
                return;
            };
            let entity_id = entity.entity_id();
            let _ = cx.with_window(entity_id, move |_, cx| {
                entity.update(cx, move |this, cx| {
                    if !this.tab_is_current(id, &current_view) {
                        return;
                    }
                    // 正常结束由终端的 CloseTerminal 收尾，这里只处理失败。
                    let Err(error) = result else {
                        return;
                    };
                    let message = if started {
                        format!(
                            "连接中断：Telnet {addr}（{}）",
                            crate::session::error_summary(&error)
                        )
                    } else {
                        format!(
                            "连接失败：Telnet {addr}（{}）",
                            crate::session::error_summary(&error)
                        )
                    };
                    this.fail_session_tab(id, message.into(), started, cx);
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 回车重连：只在焦点落在「已断开的终端」上、且没有弹窗时生效。
    ///
    /// 断开的终端仍持有焦点，但按键已无处可写；在捕获阶段拦截回车，
    /// 与 WindTerm「Remote channel is closed，按回车重新连接」的行为对齐。
    pub(super) fn capture_session_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key != "enter" || self.overlay_open() {
            return;
        }
        let Some(id) = self.active_tab else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        let reconnectable = tab.ended && tab.resume.is_some();
        let terminal_focused = tab
            .terminal_view
            .as_ref()
            .is_some_and(|view| view.read(cx).focus_handle(cx).is_focused(window));
        if !reconnectable || !terminal_focused {
            return;
        }
        cx.stop_propagation();
        self.reconnect_tab(id, window, cx);
    }

    /// 密码输入面板。
    pub(super) fn render_pending_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(pending) = self.pending.as_ref() else {
            return div().into_any_element();
        };
        let host = pending.host.addr();
        let hint = match pending.kind {
            PromptKind::Password => format!("连接 {host} 需要密码"),
            PromptKind::Passphrase => format!(
                "连接 {host} 的私钥已加密，需要口令（{}）",
                if pending.host.key_inline {
                    "内嵌私钥"
                } else if pending.host.key_path.is_empty() {
                    "未填路径"
                } else {
                    &pending.host.key_path
                }
            ),
        };
        let hint = if self.vault.is_some() {
            hint
        } else {
            format!("{hint}（加密库不可用，仅本次使用，不保存）")
        };
        let save_label = match pending.kind {
            PromptKind::Password => "保存密码（加密存入本地凭据库）",
            PromptKind::Passphrase => "保存口令（加密存入本地凭据库）",
        };
        let save_row = if self.vault.is_some() && pending.host.id != 0 {
            let checked = pending.save_password;
            Some(
                h_flex()
                    .id("save-password")
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(pending) = this.pending.as_mut() {
                            pending.save_password = !pending.save_password;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .text_sm()
                            .text_color(if checked { pal.text } else { pal.muted })
                            .child(if checked { "☑" } else { "☐" }),
                    )
                    .child(div().text_sm().text_color(pal.muted).child(save_label)),
            )
        } else {
            None
        };
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(pal.card_bg)
            .child(div().text_sm().child(hint))
            .child(styled_input(
                Input::new(&pending.password)
                    .content_type(InputContentType::Password)
                    .mask_toggle(),
                &pal,
            ))
            .children(save_row)
            .child(
                h_flex()
                    .gap_2()
                    .child(Button::new("connect").label("连接").primary().on_click(
                        cx.listener(|this, _, window, cx| this.confirm_pending(window, cx)),
                    ))
                    .child(
                        Button::new("cancel-connect")
                            .label("取消")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_pending(cx))),
                    ),
            )
            .into_any_element()
    }
}
