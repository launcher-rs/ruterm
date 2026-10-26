//! RuLink：基于 rgpui + rgpui-term 外接 IO 后端的 SSH 客户端。
//!
//! 形态为 GUI 会话管理器：无参数启动即进入主界面，左侧是持久化的主机账号列表，
//! 点击账号即连接；密码认证的账号在连接时临时输入密码（绝不落盘），私钥/Agent
//! 认证直接连接。仍兼容一次性命令行快速连接（带目标参数时首启动即连上该主机）。

mod config;
mod remote;
mod session;
mod sessions;
mod ssh;
mod vault;

use std::collections::HashMap;
use std::sync::Arc;

use rgpui::components::split_pane::{CollapsiblePane, SplitPane, SplitPaneState};
use rgpui::components::status_bar::{StatusBar, StatusBarItem};
use rgpui::input_ui::{Input, InputContentType, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, App, Bounds, Button, ButtonVariants, Context, Entity, FocusHandle, Focusable,
    FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Render,
    ScrollableElement, SharedString, StatefulInteractiveElement, Styled, TrayMenuItem, Window,
    WindowBounds, WindowOptions, div, h_flex, px, size, v_flex,
};
use rgpui_term::{TerminalBuilder, TerminalView};

use crate::config::SshTarget;
use crate::sessions::{AuthMethod, HostConfig, SessionStore};
use crate::vault::Vault;

/// 认证方式的展示名。
fn auth_label(method: AuthMethod) -> &'static str {
    match method {
        AuthMethod::Agent => "SSH Agent",
        AuthMethod::IdentityFile => "私钥",
        AuthMethod::Password => "密码",
    }
}

/// 一个已连接的会话标签（持有终端视图）。
struct SessionTab {
    /// 稳定标签 id（会话结束或用户关闭时据此定位）。
    id: usize,
    /// 标签标题（主机名）。
    title: SharedString,
    /// 连接位置（user@host:port），状态栏展示；将来远程指标也挂在这里。
    addr: SharedString,
    /// 认证方式，状态栏展示。
    auth: AuthMethod,
    /// 远程服务器基本信息摘要（连接后经 exec 探测异步填充，探测失败保持 None）。
    remote_info: Option<SharedString>,
    /// 渲染用终端视图。
    terminal_view: Entity<TerminalView>,
}

/// 新建/编辑主机表单（各字段为受控输入状态）。
struct NewHostForm {
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    key_path: Entity<InputState>,
    /// 加密私钥口令（可选）。保存时勾选入库则写加密库，否则仅本次内存缓存；绝不落明文。
    passphrase: Entity<InputState>,
    /// 表单层"保存口令到加密库"开关（默认否＝仅本次内存持有）。
    save_secret: bool,
    auth: AuthMethod,
    /// Some(index)＝编辑既有主机（保存时按该下标原位替换）。
    editing: Option<usize>,
}

/// 主机列表右键菜单（真实下标 + 弹出位置，窗口坐标）。
struct HostContextMenu {
    index: usize,
    x: Pixels,
    y: Pixels,
}

/// 弹窗索取的秘密类型：登录密码或加密私钥的口令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptKind {
    Password,
    Passphrase,
}

/// 待输入密码/口令后连接的会话（点击需要秘密的主机时进入）。
struct PendingConnect {
    host: HostConfig,
    password: Entity<InputState>,
    /// 是否勾选"保存"（仅加密库可用时生效，默认否＝本次内存持有）。
    save_password: bool,
    /// 弹窗类型（决定文案与连接参数落点）。
    kind: PromptKind,
}

/// RuLink 主视图：左侧主机账号管理，右侧会话标签终端。
struct RuLinkApp {
    /// 会话仓库（负责持久化）。
    store: SessionStore,
    /// 加密凭据库；None 表示启动引导中或已降级为旧 JSON 存储（降级时不保存密码）。
    vault: Option<Arc<Vault>>,
    /// 已保存的主机账号列表。
    hosts: Vec<HostConfig>,
    /// 主机列表搜索框（按展示名 / user@host:port 子串过滤）。
    search: Entity<InputState>,
    /// 活动会话标签。
    tabs: Vec<SessionTab>,
    /// 当前选中标签 id。
    active_tab: Option<usize>,
    /// 打开的新建主机表单（None 显示列表）。
    new_host: Option<NewHostForm>,
    /// 等待密码输入的连接。
    pending: Option<PendingConnect>,
    /// 主机行右键菜单（编辑/删除入口）。
    context_menu: Option<HostContextMenu>,
    /// 删除二次确认（真实下标；浮层遮挡期间列表不会变）。
    confirm_delete: Option<usize>,
    /// 本次运行内缓存的密码（按 user@host:port），避免重复弹窗；绝不落盘。
    password_cache: HashMap<String, String>,
    /// 标签自增 id。
    next_id: usize,
    /// 状态栏左侧的瞬态消息（连接/保存/删除/断开等节点更新）。
    status: SharedString,
    /// 侧边栏 ↔ 主区 的分隔面板状态（拖拽调整宽度 + 折叠/展开）。
    split_state: Entity<SplitPaneState>,
    focus_handle: FocusHandle,
}

/// 主题调色板：从 rgpui 全局 `Theme` 的语义色映射而来（不硬编码颜色）。
/// 切换明暗只调 `Theme::change`，本结构随之重取，界面整体换色，且与按钮 / 状态栏等
/// 组件共用同一份主题（避免手工配色与框架主题脱节）。
struct Palette {
    /// 窗口/编辑区底色。
    window_bg: Hsla,
    /// 侧边栏与标签栏底色。
    sidebar_bg: Hsla,
    /// 卡片/表单/非活动标签底色。
    card_bg: Hsla,
    /// 卡片悬停底色。
    card_hover: Hsla,
    /// 分隔边框色。
    border: Hsla,
    /// 主文字色。
    text: Hsla,
    /// 次要/弱化文字色。
    muted: Hsla,
    /// 认证方式链接色。
    link: Hsla,
    /// 删除/关闭等危险操作色。
    danger: Hsla,
    /// 选中/主按钮强调色。
    accent: Hsla,
    /// 未选中分段（认证方式切换）底色。
    segment_bg: Hsla,
    /// 输入框底色。
    input_bg: Hsla,
    /// 输入框文字色。
    input_text: Hsla,
    /// 输入框边框色。
    input_border: Hsla,
}

impl Palette {
    /// 从当前全局主题取语义色，组装一次性调色板。
    fn from_theme(cx: &App) -> Self {
        let theme = cx.theme();
        Palette {
            window_bg: theme.background,
            sidebar_bg: theme.sidebar,
            card_bg: theme.popover,
            card_hover: theme.list_hover,
            border: theme.border,
            text: theme.foreground,
            muted: theme.muted_foreground,
            link: theme.link,
            danger: theme.danger,
            accent: theme.primary,
            segment_bg: theme.muted,
            input_bg: theme.input_background(),
            input_text: theme.foreground,
            input_border: theme.border,
        }
    }
}

impl RuLinkApp {
    /// 载入已保存主机，可选执行命令行快速连接。
    fn new(quick: Option<SshTarget>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = SessionStore::new();
        let hosts = store.load();
        // 侧边栏 ↔ 主区 分隔面板：初始侧边栏占比约 0.25。
        let split_state = cx.new(SplitPaneState::new);
        split_state.update(cx, |state, cx| {
            state.set_ratio(0.25, cx);
            // WindTerm 式拖动：拖到最小宽度即停住，分隔线仍可抓住往大拖；
            // 完全收起交给标题栏 « 折叠按钮 / Ctrl+B，不靠拖拽阈值自动折叠。
            state.set_min_first(160.0, cx);
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索主机…"));
        cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        let mut app = Self {
            store,
            vault: None,
            hosts,
            search,
            tabs: Vec::new(),
            active_tab: None,
            new_host: None,
            pending: None,
            context_menu: None,
            confirm_delete: None,
            password_cache: HashMap::new(),
            next_id: 0,
            status: "就绪".into(),
            split_state,
            focus_handle: cx.focus_handle(),
        };
        app.boot_vault(cx);
        if let Some(target) = quick {
            let title = target.addr();
            app.connect(title, target, None, None, false, window, cx);
        }
        app
    }

    /// 异步打开加密凭据库：成功则接管主机列表（含一次性迁移旧 JSON 并改名保留），
    /// 失败则降级回旧 JSON 存储并提示（降级模式下密码只留在内存）。
    fn boot_vault(&mut self, cx: &mut Context<Self>) {
        let dir = self.store.dir();
        let json_hosts = self.hosts.clone();
        cx.spawn(async move |this, cx| {
            let opened = async {
                let vault = Vault::open(&dir).await?;
                // 旧 JSON 非空且库还是空的 → 一次性导入（id 顺序分配），验证后才改名。
                if !json_hosts.is_empty() && vault.count().await? == 0 {
                    let mut imported = json_hosts.clone();
                    for (index, host) in imported.iter_mut().enumerate() {
                        host.id = index as i64 + 1;
                    }
                    vault.import_hosts(&imported).await?;
                    Ok::<_, anyhow::Error>((vault, imported))
                } else {
                    let hosts = vault.load_hosts().await?;
                    Ok((vault, hosts))
                }
            }
            .await;
            let _ = this.update(cx, |this, cx| match opened {
                Ok((vault, mut hosts)) => {
                    let vault = Arc::new(vault);
                    // 开库完成前用户新增的主机（id==0）补发 id 并入库，避免丢失。
                    let mut next = hosts.iter().map(|host| host.id).max().unwrap_or(0) + 1;
                    let mut late = Vec::new();
                    for host in &this.hosts {
                        if host.id == 0 {
                            let mut host = host.clone();
                            host.id = next;
                            next += 1;
                            late.push(host);
                        }
                    }
                    for host in late {
                        hosts.push(host.clone());
                        let vault = vault.clone();
                        cx.spawn(async move |_, _| {
                            if let Err(error) = vault.upsert(&host).await {
                                log::warn!("凭据库写入失败: {error}");
                            }
                        })
                        .detach();
                    }
                    this.hosts = hosts;
                    this.vault = Some(vault);
                    this.store.mark_migrated();
                    cx.notify();
                }
                Err(error) => {
                    log::warn!("加密凭据库不可用: {error}");
                    this.status = SharedString::from(format!(
                        "加密凭据库不可用（{error}），沿用旧存储且不保存密码"
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 持久化一台主机：加密库可用则只写该行，否则整表回写旧 JSON。
    fn persist_host(&mut self, host: &HostConfig, cx: &mut Context<Self>) {
        if let Some(vault) = self.vault.clone() {
            let host = host.clone();
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.upsert(&host).await {
                    log::warn!("凭据库写入失败: {error}");
                }
            })
            .detach();
        } else {
            self.store.save(&self.hosts);
        }
    }

    /// 打开新建主机表单。
    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form_at(None, window, cx);
    }

    /// 打开新建 / 编辑表单；edit_index 为 Some 时以该主机字段预填。
    fn open_form_at(
        &mut self,
        edit_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = edit_index.and_then(|index| self.hosts.get(index).cloned());
        let editing = existing.is_some();
        let auth = existing
            .as_ref()
            .map(|host| host.auth)
            .unwrap_or(AuthMethod::Agent);
        let edit_index = if editing { edit_index } else { None };
        let placeholder_name = if editing {
            ""
        } else {
            "展示名（留空用主机地址）"
        };
        let form = NewHostForm {
            name: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder(placeholder_name);
                match &existing {
                    Some(host) => input.default_value(host.name.clone()),
                    None => input,
                }
            }),
            host: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder("host 或 IP");
                match &existing {
                    Some(host) => input.default_value(host.host.clone()),
                    None => input,
                }
            }),
            port: cx.new(|cx| {
                let input = InputState::new(window, cx).default_value("22");
                match &existing {
                    Some(host) => InputState::new(window, cx).default_value(host.port.to_string()),
                    None => input,
                }
            }),
            user: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder("登录用户名");
                match &existing {
                    Some(host) => input.default_value(host.user.clone()),
                    None => input,
                }
            }),
            key_path: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder("~/.ssh/id_ed25519");
                match &existing {
                    Some(host) => input.default_value(host.key_path.clone()),
                    None => input,
                }
            }),
            // 口令永不回填：编辑时留空即"不改动已保存的秘密"。
            passphrase: cx.new(|cx| {
                InputState::new(window, cx).placeholder("私钥口令（可选，留空则连接时按需询问）")
            }),
            save_secret: false,
            auth,
            editing: edit_index,
        };
        self.new_host = Some(form);
        // 表单打开即把焦点交给"主机地址"输入框：退格/删除等键位动作只在输入框
        // 持有焦点（"Input" 键位上下文激活）时才会派发，主动聚焦让删除键即时可用。
        if let Some(form) = self.new_host.as_ref() {
            form.host.update(cx, |state, cx| state.focus(window, cx));
        }
        self.status = if editing {
            "编辑主机".into()
        } else {
            "填写主机信息".into()
        };
        cx.notify();
    }

    /// 关闭新建表单（不保存）。
    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        self.new_host = None;
        self.status = "就绪".into();
        cx.notify();
    }

    /// 保存新建/编辑表单到列表并持久化；校验失败则保留表单。
    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.new_host.as_ref() else {
            return;
        };
        let editing = form.editing;
        let auth = form.auth;
        let passphrase = form.passphrase.read(cx).value().trim().to_string();
        let save_secret = form.save_secret;
        let host_raw = form.host.read(cx).value().trim().to_string();
        let mut host = HostConfig {
            id: 0,
            name: form.name.read(cx).value().trim().to_string(),
            host: host_raw.clone(),
            port: form
                .port
                .read(cx)
                .value()
                .trim()
                .parse::<u16>()
                .unwrap_or(22),
            user: form.user.read(cx).value().trim().to_string(),
            auth,
            key_path: form.key_path.read(cx).value().trim().to_string(),
            favorite: false,
        };
        if let Some(error) = host.validate() {
            log::warn!("保存主机失败: {error}");
            self.status = SharedString::from(format!("保存失败：{error}"));
            cx.notify();
            return;
        }
        let label = if host.name.is_empty() {
            host.host.clone()
        } else {
            host.name.clone()
        };
        if let Some(index) = editing {
            // 原位替换：id 与收藏标记沿用原主机；已保存的密码/口令不受影响。
            if let Some(current) = self.hosts.get(index).cloned() {
                host.id = current.id;
                host.favorite = current.favorite;
                self.hosts[index] = host.clone();
                self.persist_host(&host, cx);
                let note = self.store_form_secret(&host, passphrase.clone(), save_secret, cx);
                self.new_host = None;
                self.status = SharedString::from(format!(
                    "已更新主机 {label}{}",
                    note.as_deref().unwrap_or("")
                ));
                cx.notify();
                return;
            }
        }
        // 新建：加密库可用时同步分配主键（max+1，与库内 INSERT 显式 id 对齐）；
        // 降级模式沿用旧 JSON（id=0，boot_vault 成功回填时会补发）。
        if self.vault.is_some() {
            host.id = self.hosts.iter().map(|item| item.id).max().unwrap_or(0) + 1;
        }
        self.hosts.push(host.clone());
        self.persist_host(&host, cx);
        let note = self.store_form_secret(&host, passphrase, save_secret, cx);
        self.new_host = None;
        self.status = SharedString::from(format!(
            "已保存主机 {label}{}",
            note.as_deref().unwrap_or("")
        ));
        cx.notify();
    }

    /// 表单私钥口令的两档落点：勾选入库（加密库）或仅本次内存缓存。
    /// 空白口令＝不改动既有秘密。返回附注文案（None 表示无需提示）。
    fn store_form_secret(
        &mut self,
        host: &HostConfig,
        passphrase: String,
        save: bool,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if host.auth != AuthMethod::IdentityFile || passphrase.is_empty() {
            return None;
        }
        if save {
            if let (Some(vault), true) = (self.vault.clone(), host.id != 0) {
                let id = host.id;
                self.password_cache.insert(host.addr(), passphrase.clone());
                cx.spawn(async move |_, _| {
                    if let Err(error) = vault.set_password(id, Some(&passphrase)).await {
                        log::warn!("保存私钥口令失败: {error}");
                    }
                })
                .detach();
                return Some("（口令已加密入库）".to_string());
            }
            // 加密库不可用：绝不降级为明文保存，退回本次内存持有。
            self.password_cache.insert(host.addr(), passphrase);
            return Some("（加密库不可用，口令仅本次内存持有）".to_string());
        }
        self.password_cache.insert(host.addr(), passphrase);
        Some("（口令仅本次内存持有）".to_string())
    }

    /// 打开系统文件对话框选择私钥文件，回填表单"私钥路径"。
    fn browse_key_path(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.new_host.as_ref() else {
            return;
        };
        let input = form.key_path.clone();
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择私钥文件".into()),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.first() {
                    let value = path.to_string_lossy().into_owned();
                    let _ = cx.with_window(entity_id, move |window, cx| {
                        let _ = input.update(cx, |state, cx| {
                            state.set_value(value, window, cx);
                        });
                    });
                }
            }
        })
        .detach();
    }

    /// 右键菜单"删除"：进入二次确认，不直接动手。
    fn ask_delete(&mut self, index: usize, cx: &mut Context<Self>) {
        self.context_menu = None;
        if index < self.hosts.len() {
            self.confirm_delete = Some(index);
            cx.notify();
        }
    }

    /// 确认删除：走真实删除并持久化（连带清除库内该主机的密码/口令）。
    fn confirm_delete_now(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.confirm_delete.take() else {
            return;
        };
        self.delete_host(index, cx);
    }

    /// 取消删除确认。
    fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        cx.notify();
    }

    /// 删除指定主机并持久化。
    fn delete_host(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.hosts.len() {
            let removed = self.hosts.remove(index);
            let label = removed.name.clone();
            if let Some(vault) = self.vault.clone() {
                let id = removed.id;
                cx.spawn(async move |_, _| {
                    if let Err(error) = vault.remove(id).await {
                        log::warn!("凭据库删除失败: {error}");
                    }
                })
                .detach();
            } else {
                self.store.save(&self.hosts);
            }
            self.status = SharedString::from(format!("已删除主机 {label}"));
            cx.notify();
        }
    }

    /// 切换指定主机的收藏状态并持久化。
    fn toggle_favorite(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(host) = self.hosts.get_mut(index) {
            host.favorite = !host.favorite;
            let host = host.clone();
            self.persist_host(&host, cx);
            cx.notify();
        }
    }

    /// 点击主机账号：密码/私钥口令按 内存缓存 → 加密库 → 弹窗 的顺序取得；Agent 直接连接。
    fn click_host(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get(index).cloned() else {
            return;
        };
        match host.auth {
            AuthMethod::Agent => {
                let target = SshTarget::from_host(&host, None);
                let title = host.name.clone();
                self.connect(title, target, None, None, false, window, cx);
            }
            AuthMethod::Password | AuthMethod::IdentityFile => {
                self.click_secret_host(host, window, cx);
            }
        }
    }

    /// 密码/私钥主机的取密顺序：缓存 → 加密库 →（密码）弹窗 /（私钥）免口令直连探测。
    fn click_secret_host(&mut self, host: HostConfig, window: &mut Window, cx: &mut Context<Self>) {
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
            let entity = cx.entity();
            let entity_id = entity.entity_id();
            cx.spawn(async move |_, cx| {
                let saved = vault.password_of(host.id).await.ok().flatten();
                let _ = cx.with_window(entity_id, move |window, cx| {
                    let _ = entity.update(cx, move |this, cx| {
                        if this.pending.is_some() {
                            return;
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

    /// 用取到的秘密（缓存或加密库）建立连接目标并连接。
    fn connect_with_secret(
        &mut self,
        host: HostConfig,
        secret: String,
        key: String,
        from_vault: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match host.auth {
            AuthMethod::Password => SshTarget::from_host(&host, Some(secret)),
            _ => SshTarget::from_host(&host, None).with_passphrase(Some(secret)),
        };
        let title = host.name.clone();
        self.connect(title, target, Some(key), Some(host), from_vault, window, cx);
    }

    /// 不带秘密直连（私钥主机的口令探测路径）。
    fn connect_without_secret(
        &mut self,
        host: HostConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = SshTarget::from_host(&host, None);
        let title = host.name.clone();
        self.connect(title, target, None, Some(host), false, window, cx);
    }

    /// 弹出密码/私钥口令输入面板（默认不保存；加密库可用时提供"保存"勾选）。
    fn prompt_secret(
        &mut self,
        host: HostConfig,
        kind: PromptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
    fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        cx.notify();
    }

    /// 用输入的秘密完成待连接：内存缓存必写；勾选保存且加密库可用时再持久化入库。
    fn confirm_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            PromptKind::Password => SshTarget::from_host(&host, Some(secret)),
            PromptKind::Passphrase => {
                SshTarget::from_host(&host, None).with_passphrase(Some(secret))
            }
        };
        self.connect(
            host.name.clone(),
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
    fn connect(
        &mut self,
        title: String,
        target: SshTarget,
        cache_key: Option<String>,
        host: Option<HostConfig>,
        secret_from_vault: bool,
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
        let terminal_view = cx.new(|cx| TerminalView::new(terminal, window, cx));
        // 状态栏展示用的连接位置与认证方式，须在 target 被 session::start 消费前取出。
        let addr = format!("{}@{}:{}", target.user, target.host, target.port);
        let auth = if target.password.is_some() {
            AuthMethod::Password
        } else if target.key_path.is_some() {
            AuthMethod::IdentityFile
        } else {
            AuthMethod::Agent
        };
        let id = self.next_id;
        self.next_id += 1;
        let session::SshSession {
            join,
            remote_info,
            started,
        } = session::start(target, handles, cx);
        self.status = SharedString::from(format!("正在连接 {addr}…"));
        self.tabs.push(SessionTab {
            id,
            title: title.into(),
            addr: addr.clone().into(),
            auth,
            remote_info: None,
            terminal_view,
        });
        self.active_tab = Some(id);
        let vault = self.vault.clone();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let shell_started = started.load(std::sync::atomic::Ordering::SeqCst);
            let Some(entity) = this.upgrade() else {
                return;
            };
            let entity_id = entity.entity_id();
            let _ = cx.with_window(entity_id, move |window, cx| {
                let _ = entity.update(cx, move |this, cx| {
                    let Err(error) = result else {
                        this.remove_tab(id, cx);
                        return;
                    };
                    // 加密私钥缺口令：回收死标签，自动弹口令窗。
                    if error
                        .downcast_ref::<session::PassphraseRequired>()
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
                    let rejected = error.downcast_ref::<session::SecretRejected>().is_some();
                    if rejected {
                        if let Some(key) = &cache_key {
                            this.password_cache.remove(key);
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
                    if !shell_started {
                        // 从未建立终端：保留标签页，错误详情已写入终端可继续阅读。
                        this.status = SharedString::from(format!("连接失败：{addr}"));
                        cx.notify();
                        return;
                    }
                    this.remove_tab(id, cx);
                    this.status = SharedString::from(format!(
                        "连接失败：{addr}（{}）",
                        session::error_summary(&error)
                    ));
                    cx.notify();
                });
            });
        })
        .detach();
        // 远程基本信息比会话短得多：探测回来时标签可能已关闭，据此判断后再写入。
        cx.spawn(async move |this, cx| {
            if let Ok(info) = remote_info.await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == id) {
                        tab.remote_info = Some(SharedString::from(info));
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// 用户点击标签关闭按钮。
    fn close_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        self.remove_tab(id, cx);
    }

    /// 移除标签并修正选中态；顺带把状态栏切到"已断开"。
    fn remove_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        let title = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.title.clone());
        let before = self.tabs.len();
        self.tabs.retain(|tab| tab.id != id);
        if self.tabs.len() == before {
            return;
        }
        if let Some(title) = title {
            self.status = SharedString::from(format!("已断开 {title}"));
        }
        if self.active_tab == Some(id) {
            self.active_tab = self.tabs.last().map(|tab| tab.id);
        }
        cx.notify();
    }

    /// 当前主题对应的调色板（从全局 Theme 语义色组装）。
    fn palette(&self, cx: &App) -> Palette {
        Palette::from_theme(cx)
    }

    /// 切换明暗主题：翻转全局 Theme 模式并持久化，界面（含状态栏/按钮）随之重取配色。
    fn toggle_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = cx.theme().is_dark();
        let mode = if dark {
            rgpui::theme::ThemeMode::Light
        } else {
            rgpui::theme::ThemeMode::Dark
        };
        rgpui::theme::Theme::change(mode, Some(window), cx);
        self.store.save_theme(!dark);
        cx.notify();
    }
}

impl Render for RuLinkApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = self.palette(cx);
        v_flex()
            .id("rulink-root")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(pal.window_bg)
            .text_color(pal.text)
            .on_key_down(
                cx.listener(|this, event: &rgpui::KeyDownEvent, window, cx| {
                    // Ctrl+B 折叠 / 展开侧边栏（走分隔面板状态）。
                    if event.keystroke.key == "b" && event.keystroke.modifiers.control {
                        this.split_state.update(cx, |state, cx| {
                            state.toggle_collapse(CollapsiblePane::First, cx);
                        });
                        cx.notify();
                    }
                    // Ctrl+T 切换明暗主题。
                    if event.keystroke.key == "t" && event.keystroke.modifiers.control {
                        this.toggle_theme(window, cx);
                    }
                }),
            )
            .child(
                SplitPane::horizontal(self.split_state.clone())
                    .show_collapse_buttons(false)
                    .first(self.render_sidebar(cx))
                    .second(self.render_main(cx))
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden(),
            )
            .child(self.render_status_bar().flex_shrink_0().w_full())
            .child(self.render_overlay(window, cx))
    }
}

impl RuLinkApp {
    /// 左侧栏：标题 + 主题切换 / 新建按钮 / 主机列表面板。
    /// 宽度与折叠由外层 SplitPane 负责，这里只填充第一面板区域。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let panel = if self.new_host.is_some() {
            self.render_form_panel(cx)
        } else if self.pending.is_some() {
            self.render_pending_panel(cx)
        } else {
            self.render_host_list(cx)
        };

        let dark = cx.theme().is_dark();
        let theme_label: &'static str = if dark { "☀" } else { "☾" };
        let theme_tooltip: &'static str = if dark {
            "切换到亮色主题 (Ctrl+T)"
        } else {
            "切换到暗色主题 (Ctrl+T)"
        };
        // 搜索框只在主机列表模式下出现（表单 / 待输入密码时隐藏）。
        let list_mode = self.new_host.is_none() && self.pending.is_none();

        v_flex()
            .size_full()
            .bg(pal.sidebar_bg)
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(rgpui::FontWeight(700.0))
                            .child("RuLink"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("toggle-theme")
                                    .label(theme_label)
                                    .ghost()
                                    .tooltip(theme_tooltip)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_theme(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("sidebar-collapse")
                                    .label("«")
                                    .ghost()
                                    .tooltip("折叠侧边栏 (Ctrl+B)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.split_state.update(cx, |state, cx| {
                                            state.collapse(CollapsiblePane::First, cx);
                                        });
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                Button::new("new-host")
                    .label("+ 新建主机")
                    .primary()
                    .w_full()
                    .on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))),
            )
            .when(list_mode, |this| {
                this.child(styled_input(Input::new(&self.search), &pal))
            })
            .child(div().flex_1().overflow_y_scrollbar().child(panel))
            .into_any_element()
    }

    /// 窗口级浮层：主机右键菜单与删除二次确认框（绝对定位渲染在最上层）。
    fn render_overlay(&self, window: &mut Window, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        // 删除二次确认：半透明遮罩 + 居中卡片，点遮罩或"取消"关闭。
        if let Some(index) = self.confirm_delete {
            let label = self
                .hosts
                .get(index)
                .map(|host| {
                    if host.name.is_empty() {
                        host.addr()
                    } else {
                        host.name.clone()
                    }
                })
                .unwrap_or_else(|| "该主机".to_string());
            return v_flex()
                .id("delete-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .items_center()
                .justify_center()
                .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.cancel_delete(cx);
                    }),
                )
                .child(
                    v_flex()
                        .id("delete-dialog")
                        .w(px(340.0))
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
                                .child(format!("删除主机 {label}？")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(pal.muted)
                                .child("若该主机已保存密码/口令，将一并从加密库中清除。"),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(Button::new("cancel-delete").label("取消").ghost().on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.cancel_delete(cx);
                                    }),
                                ))
                                .child(
                                    Button::new("confirm-delete")
                                        .label("删除")
                                        .danger()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_delete_now(cx);
                                        })),
                                ),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| {
                                // 阻止卡片内点击冒泡到遮罩（否则点对话框也会关闭）。
                                cx.stop_propagation();
                            }),
                        ),
                )
                .into_any_element();
        }
        // 主机行右键菜单：编辑 / 删除（删除走二次确认）。
        if let Some(menu) = self.context_menu.as_ref() {
            let index = menu.index;
            let bounds = window.bounds();
            let x = menu.x.min(bounds.size.width - px(150.0)).max(px(0.0));
            let y = menu.y.min(bounds.size.height - px(100.0)).max(px(0.0));
            let close = cx.listener(|this, _, _, cx| {
                this.context_menu = None;
                cx.notify();
            });
            return div()
                .id("menu-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .on_mouse_down(MouseButton::Left, close)
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        this.context_menu = None;
                        cx.notify();
                    }),
                )
                .child(
                    v_flex()
                        .id("host-context-menu")
                        .absolute()
                        .top(y)
                        .left(x)
                        .min_w(px(140.0))
                        .py_1()
                        .rounded(px(6.0))
                        .bg(pal.card_bg)
                        .border_1()
                        .border_color(pal.border)
                        .child(
                            div()
                                .id("ctx-edit-host")
                                .px_3()
                                .py_1()
                                .text_sm()
                                .cursor_pointer()
                                .hover(|style| style.bg(pal.card_hover))
                                .child("编辑")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.context_menu = None;
                                    this.open_form_at(Some(index), window, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("ctx-delete-host")
                                .px_3()
                                .py_1()
                                .text_sm()
                                .text_color(pal.danger)
                                .cursor_pointer()
                                .hover(|style| style.bg(pal.card_hover))
                                .child("删除")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.ask_delete(index, cx);
                                })),
                        ),
                )
                .into_any_element();
        }
        div().into_any_element()
    }

    /// 主机账号列表；每行可点击连接、附删除按钮。
    fn render_host_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        if self.hosts.is_empty() {
            return div()
                .text_sm()
                .text_color(pal.muted)
                .child("暂无主机，点击上方“新建主机”添加。")
                .into_any_element();
        }
        let query = self.search.read(cx).value().trim().to_lowercase();
        // 按展示名 / user@host:port 子串过滤；保留真实下标供点击/删除定位。
        let mut matched: Vec<usize> = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, host)| {
                query.is_empty()
                    || host.name.to_lowercase().contains(&query)
                    || host.addr().to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        // 收藏主机置顶（稳定排序，同组内保持原插入顺序）。
        matched.sort_by_key(|&index| !self.hosts[index].favorite);
        if matched.is_empty() {
            return div()
                .text_sm()
                .text_color(pal.muted)
                .child("无匹配主机")
                .into_any_element();
        }
        v_flex()
            .gap_1()
            .children(matched.into_iter().map(|index| {
                let host = &self.hosts[index];
                let host_id = SharedString::from(format!("host-row-{index}"));
                let fav_id = SharedString::from(format!("host-fav-{index}"));
                let fav_glyph: &'static str = if host.favorite { "★" } else { "☆" };
                h_flex()
                    .id(host_id)
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .rounded(px(6.0))
                    .bg(pal.card_bg)
                    .hover(|style| style.bg(pal.card_hover))
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.click_host(index, window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &rgpui::MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.context_menu = Some(HostContextMenu {
                                index,
                                x: event.position.x,
                                y: event.position.y,
                            });
                            cx.notify();
                        }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_ellipsis()
                                    .font_weight(rgpui::FontWeight(600.0))
                                    .child(host.name.clone()),
                            )
                            .child(
                                div()
                                    .text_ellipsis()
                                    .text_xs()
                                    .text_color(pal.muted)
                                    .child(host.addr()),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .id(fav_id)
                                    .text_sm()
                                    .px_1()
                                    .text_color(if host.favorite { pal.accent } else { pal.muted })
                                    .child(fav_glyph)
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_favorite(index, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(pal.link)
                                    .child(auth_label(host.auth)),
                            ),
                    )
            }))
            .into_any_element()
    }

    /// 新建主机表单面板。
    fn render_form_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(form) = self.new_host.as_ref() else {
            return div().into_any_element();
        };
        let methods = [
            AuthMethod::Agent,
            AuthMethod::IdentityFile,
            AuthMethod::Password,
        ];
        let selected = form.auth;
        let editing = form.editing.is_some();
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(pal.card_bg)
            .child(
                div()
                    .text_sm()
                    .font_weight(rgpui::FontWeight(600.0))
                    .child(if editing {
                        "编辑主机"
                    } else {
                        "新建主机"
                    }),
            )
            .child(labeled_field("展示名", &form.name, &pal))
            .child(labeled_field("主机地址", &form.host, &pal))
            .child(labeled_field("端口", &form.port, &pal))
            .child(labeled_field("用户名", &form.user, &pal))
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child("认证方式"))
                    .child(h_flex().gap_2().children(methods.into_iter().map(|method| {
                        let auth_id = SharedString::from(format!("auth-{}", auth_label(method)));
                        let is_selected = method == selected;
                        div()
                            .id(auth_id)
                            .px_3()
                            .py_1()
                            .rounded(px(5.0))
                            .text_sm()
                            .cursor_pointer()
                            .bg(if is_selected {
                                pal.accent
                            } else {
                                pal.segment_bg
                            })
                            // 选中段文字用主题的 primary_foreground（与主按钮同规则），
                            // 否则强调色底上会沿用默认文字色导致看不清。
                            .text_color(if is_selected {
                                cx.theme().primary_foreground
                            } else {
                                pal.text
                            })
                            .when(is_selected, |this| this.font_weight(FontWeight::BOLD))
                            .child(auth_label(method))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = this.new_host.as_mut() {
                                    form.auth = method;
                                }
                                cx.notify();
                            }))
                    }))),
            )
            .when(selected == AuthMethod::IdentityFile, |this| {
                let save_checked = form.save_secret;
                this.child(
                    h_flex()
                        .gap_2()
                        .items_end()
                        .child(div().flex_1().child(labeled_field(
                            "私钥路径",
                            &form.key_path,
                            &pal,
                        )))
                        .child(
                            Button::new("browse-key")
                                .label("浏览…")
                                .ghost()
                                .flex_shrink_0()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.browse_key_path(cx);
                                })),
                        ),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().child("私钥口令"))
                        .child(styled_input(
                            Input::new(&form.passphrase).content_type(InputContentType::Password),
                            &pal,
                        )),
                )
                .when(self.vault.is_some(), |this| {
                    this.child(
                        h_flex()
                            .id("save-passphrase")
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(form) = this.new_host.as_mut() {
                                    form.save_secret = !form.save_secret;
                                    cx.notify();
                                }
                            }))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if save_checked { pal.text } else { pal.muted })
                                    .child(if save_checked { "☑" } else { "☐" }),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(pal.muted)
                                    .child("保存口令（加密存入本地凭据库，默认仅本次内存持有）"),
                            ),
                    )
                })
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save-host")
                            .label(if editing { "保存修改" } else { "保存" })
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.save_form(cx))),
                    )
                    .child(
                        Button::new("cancel-host")
                            .label("取消")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
                    ),
            )
            .into_any_element()
    }

    /// 密码输入面板。
    fn render_pending_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(pending) = self.pending.as_ref() else {
            return div().into_any_element();
        };
        let host = pending.host.addr();
        let hint = match pending.kind {
            PromptKind::Password => format!("连接 {host} 需要密码"),
            PromptKind::Passphrase => format!(
                "连接 {host} 的私钥已加密，需要口令（{}）",
                if pending.host.key_path.is_empty() {
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
                Input::new(&pending.password).content_type(InputContentType::Password),
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

    /// 右侧主区：标签栏 + 当前终端。
    fn render_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = self.palette(cx);
        let tab_bar = if self.tabs.is_empty() {
            div()
        } else {
            h_flex()
                .h(px(44.0))
                .flex_shrink_0()
                .items_center()
                .bg(pal.sidebar_bg)
                .border_b_1()
                .border_color(pal.border)
                .px_3()
                .gap_2()
                .overflow_hidden()
                .children(self.tabs.iter().map(|tab| {
                    let id = tab.id;
                    let active = self.active_tab == Some(id);
                    let tab_id = SharedString::from(format!("tab-{id}"));
                    let close_id = SharedString::from(format!("tab-close-{id}"));
                    h_flex()
                        .id(tab_id)
                        .h(px(32.0))
                        .min_w(px(160.0))
                        .flex_shrink_0()
                        .px_4()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .rounded(px(6.0))
                        .bg(if active { pal.window_bg } else { pal.card_bg })
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.active_tab = Some(id);
                            cx.notify();
                        }))
                        .child(div().child(tab.title.clone()))
                        .child(
                            div()
                                .id(close_id)
                                .text_base()
                                .text_color(pal.danger)
                                .child("×")
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.close_tab(id, cx)),
                                ),
                        )
                }))
        };

        let body = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .map(|tab| {
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(tab.terminal_view.clone())
                    .into_any_element()
            })
            .unwrap_or_else(|| {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_base()
                            .text_color(pal.muted)
                            .child("从左侧点击主机账号开始连接"),
                    )
                    .into_any_element()
            });

        // 侧边栏折叠后，主区顶部补一条展开栏（分隔面板自带折叠按钮已关闭）。
        let collapsed = self.split_state.read(cx).is_collapsed();
        let expand_bar = if collapsed {
            h_flex()
                .h(px(44.0))
                .flex_shrink_0()
                .items_center()
                .px_2()
                .bg(pal.sidebar_bg)
                .border_b_1()
                .border_color(pal.border)
                .child(
                    Button::new("sidebar-expand")
                        .label("»")
                        .ghost()
                        .tooltip("展开侧边栏 (Ctrl+B)")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.split_state.update(cx, |state, cx| state.expand(cx));
                            cx.notify();
                        })),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        v_flex()
            .flex_1()
            .h_full()
            .child(expand_bar)
            .child(tab_bar)
            .child(body)
    }

    /// 底部状态栏：左侧瞬态状态 + 活动会话信息（地址/认证/远程指标占位），右侧会话计数。
    fn render_status_bar(&self) -> StatusBar {
        let active = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id));

        let mut left = vec![StatusBarItem::new(self.status.clone()).id("status")];
        if let Some(tab) = active {
            left.push(StatusBarItem::new(tab.addr.clone()).id("addr"));
            left.push(
                StatusBarItem::new(auth_label(tab.auth))
                    .id("auth")
                    .muted(true),
            );
            // 远程基本信息由 SSH 建立后的 exec 探测任务异步写入 tab.remote_info。
            if let Some(info) = tab.remote_info.clone() {
                left.push(StatusBarItem::new(info).id("remote").muted(true));
            }
        }
        let right = vec![
            StatusBarItem::new(format!("会话 {}", self.tabs.len()))
                .id("sessions")
                .muted(true),
        ];
        StatusBar::new().left(left).right(right)
    }
}

/// 带标题的输入框行。
fn labeled_field(
    label: &'static str,
    input: &Entity<InputState>,
    pal: &Palette,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().text_sm().child(label))
        .child(styled_input(Input::new(input), pal))
}

/// 统一输入框外观：随主题取底色/文字/边框，保证未聚焦时文字与边框都清晰可辨。
fn styled_input(input: Input, pal: &Palette) -> Input {
    input
        .bg(pal.input_bg)
        .text_color(pal.input_text)
        .border_1()
        .border_color(pal.input_border)
}

impl Focusable for RuLinkApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn main() {
    rgpui::init_logging();

    // 带目标参数 → 一次性快速连接兜底；无参数 → 纯 GUI 会话管理器。
    let quick = if std::env::args().nth(1).is_some() {
        match SshTarget::from_args() {
            Ok(target) => Some(target),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    let app = rgpui_platform::application();
    app.run(move |cx| {
        rgpui::tokio::init(cx);
        // 主题 + 输入子系统必须初始化：否则输入框无配色且
        // 退格/删除等键位动作未注册（无法编辑已输入内容）。
        rgpui::theme::init(cx);
        // theme::init 默认切到亮色；这里按上次持久化的主题选择应用（缺省暗色），
        // 使输入框/界面底色与用户上次退出时的选择一致。
        let dark = SessionStore::new().load_settings().dark();
        rgpui::theme::Theme::change(
            if dark {
                rgpui::theme::ThemeMode::Dark
            } else {
                rgpui::theme::ThemeMode::Light
            },
            None,
            cx,
        );
        rgpui::input_ui::init(cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.0), px(720.0)),
                    cx,
                ))),
                titlebar: Some(rgpui::TitlebarOptions {
                    title: Some("RuLink".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let quick = quick.clone();
                let app = cx.new(|cx| RuLinkApp::new(quick, window, cx));
                // 关闭按钮改为隐藏到托盘：窗口不销毁，会话后台保活；
                // 返回 false 中断关闭，真正退出走托盘"退出"（cx.quit 直接投 WM_QUIT，不经此回调）。
                window.on_window_should_close(cx, |window, _cx| {
                    window.hide_window();
                    false
                });
                app
            },
        );
        let window_handle = match opened {
            Ok(handle) => handle,
            Err(_) => {
                cx.quit();
                return;
            }
        };
        cx.activate(true);

        // 系统托盘：品牌图标 + 悬停提示 + 右键菜单（显示主界面 / 退出）。
        // 图标以 32px PNG 内嵌，Windows 走 image 解码路径最稳；不依赖运行时文件路径。
        const TRAY_ICON: &[u8] = include_bytes!("../assets/icons/tray-32.png");
        cx.set_tray_icon(Some(TRAY_ICON));
        cx.set_tray_tooltip("RuLink 拾连");
        cx.set_tray_menu(vec![
            TrayMenuItem::Action {
                label: "显示主界面".into(),
                id: "show".into(),
            },
            TrayMenuItem::Separator,
            TrayMenuItem::Action {
                label: "退出".into(),
                id: "quit".into(),
            },
        ]);
        cx.on_tray_menu_action(move |id: SharedString, cx: &mut App| match id.as_ref() {
            "quit" => cx.quit(),
            "show" => {
                // 从托盘恢复：把隐藏的主窗口重新带到前台。
                window_handle
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
            }
            _ => {}
        });
    });
}
