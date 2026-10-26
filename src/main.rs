//! RuLink：基于 rgpui + rgpui-term 外接 IO 后端的 SSH 客户端。
//!
//! 形态为 GUI 会话管理器：无参数启动即进入主界面，左侧是持久化的主机账号列表，
//! 点击账号即连接；密码认证的账号在连接时临时输入密码（绝不落盘），私钥/Agent
//! 认证直接连接。仍兼容一次性命令行快速连接（带目标参数时首启动即连上该主机）。

mod config;
mod paths;
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
use rgpui::tabs::{Tab, TabBar};
use rgpui::title_bar::TITLE_BAR_HEIGHT;
use rgpui::{
    AnyElement, App, Bounds, Button, ButtonVariants, Context, DropdownMenu, Entity, FocusHandle,
    Focusable, FontWeight, Hsla, Icon, IconName, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, PopupMenu, PopupMenuItem, Render, ScrollableElement, SharedString,
    StatefulInteractiveElement, Styled, TrayMenuItem, Window, WindowBounds, WindowControlArea,
    WindowOptions, div, h_flex, px, size, v_flex,
};
use rgpui_term::{TerminalBuilder, TerminalView};

use crate::config::SshTarget;
use crate::paths::StorageMode;
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

/// 便携/主密码库的解锁弹窗状态（主密码派生密钥，库文件可带往他机）。
struct UnlockState {
    passphrase: Entity<InputState>,
    /// 首次建库：需要二次输入确认主密码（口令错误将无法重建库）。
    confirm: Entity<InputState>,
    is_new: bool,
    error: Option<SharedString>,
}

/// 设置弹窗状态：主密码表单（含当前口令校验）与应用偏好。
struct SettingsState {
    /// 当前分页（见 `SETTINGS_TAB_*`）。
    tab: usize,
    /// 已是口令档时要求的"当前主密码"——应用可能在托盘里挂很久，
    /// 不验证就能换密钥等于没有锁。库未解锁时不显示。
    current: Option<Entity<InputState>>,
    /// 新口令两栏：提交成功后连同输入一并丢弃，明文不留存界面里；
    /// 需要再改一次时点"更改主密码"重建输入框。
    password: Option<Entity<InputState>>,
    confirm: Option<Entity<InputState>>,
    error: Option<SharedString>,
    info: Option<SharedString>,
    /// 搬迁进行中：输入框与保存按钮禁用，避免重复触发。
    busy: bool,
}

/// 主密码操作：设置 / 更换为口令派生密钥，或取消并换回系统凭据管理器密钥。
#[derive(Clone, Copy, PartialEq, Eq)]
enum MasterPasswordAction {
    Set,
    Cancel,
}

/// 新口令输入框的提示语：口令档下这次操作是"更换"。
fn password_placeholder(passphrase_mode: bool) -> &'static str {
    if passphrase_mode {
        "新主密码"
    } else {
        "主密码（可不设置）"
    }
}

/// 设置弹窗分页：凭据库与主密码。
const SETTINGS_TAB_VAULT: usize = 0;
/// 设置弹窗分页：外观。
const SETTINGS_TAB_APPEARANCE: usize = 1;

/// RuLink 主视图：左侧主机账号管理，右侧会话标签终端。
struct RuLinkApp {
    /// 会话仓库（负责持久化）。
    store: SessionStore,
    /// 存储模式（便携 / 全局），启动时探测一次。
    mode: StorageMode,
    /// 加密凭据库；None 表示启动引导中、已降级为旧 JSON 存储，或被用户锁定。
    vault: Option<Arc<Vault>>,
    /// 本次解锁所用的库主密钥（十六进制）。只存活在内存，绝不入库、入日志、入命令行；
    /// 换主密码前用它校验"当前主密码"，锁定/退出即丢弃。
    unlocked_key: Option<String>,
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
    /// 主密码解锁弹窗（便携模式 / 已设置主密码时启动弹出）。
    unlock: Option<UnlockState>,
    /// 设置弹窗（主密码管理、主题、锁定）。
    settings: Option<SettingsState>,
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
        let mode = StorageMode::detect();
        let store = SessionStore::new(&mode);
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
            mode,
            vault: None,
            hosts,
            search,
            tabs: Vec::new(),
            active_tab: None,
            new_host: None,
            pending: None,
            context_menu: None,
            confirm_delete: None,
            unlock: None,
            settings: None,
            unlocked_key: None,
            password_cache: HashMap::new(),
            next_id: 0,
            status: "就绪".into(),
            split_state,
            focus_handle: cx.focus_handle(),
        };
        app.boot_vault(window, cx);
        if let Some(target) = quick {
            let title = target.addr();
            app.connect(title, target, None, None, false, window, cx);
        }
        app
    }

    /// 异步打开加密凭据库：成功则接管主机列表（含一次性迁移旧库/JSON 并改名保留），
    /// 失败则降级回旧 JSON 存储并提示（降级模式下密码只留在内存）。
    ///
    /// 口令档（已设置主密码）启动时弹出解锁对话框，口令错误可重试；
    /// keyring 档保持零输入直连。
    fn boot_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self.mode.clone();
        let json_hosts = self.hosts.clone();
        // 凭据库（toasty/turso）必须跑在 tokio 运行时上，句柄在此处取出后交给任务。
        let runtime = rgpui::tokio::Tokio::handle(cx);
        // 只有口令档才弹解锁框：便携模式不再强制设密码，默认可免密使用，
        // 是否配置主密码交由设置界面决定。
        let passphrase_needed =
            crate::vault::read_unlock_marker(&mode) == crate::vault::UnlockBackend::Passphrase;
        if passphrase_needed {
            self.open_unlock_dialog(window, cx);
        } else if mode.is_portable() {
            // 便携库若仍由本机凭据管理器加密，文件拷到其他机器无法解锁：只提示不拦路。
            self.status =
                "便携模式正用本机凭据管理器解锁；建议到设置中配置主密码，以便带往其他机器".into();
        }
        cx.spawn(async move |this, cx| {
            // 口令档（已设置主密码）：密钥只能来自用户输入，
            // 绝不能用 keyring 随机密钥代开——那会让主密码形同虚设。
            if passphrase_needed {
                return;
            }
            // 库文件在数据目录还是原样，先探测主密钥是否可得。
            let keyring_key = match crate::vault::load_or_create_master_key() {
                Ok(key) => Some(key),
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        log::warn!("系统凭据管理器不可用: {error}");
                        this.status =
                            SharedString::from(format!("凭据管理器不可用（{error}），不保存密码"));
                        cx.notify();
                    });
                    return;
                }
            };
            let opened = async {
                let vault =
                    Vault::open(&mode, keyring_key.clone().unwrap(), runtime.clone()).await?;
                let hosts =
                    Self::after_vault_open(&vault, &mode, keyring_key.as_deref(), &json_hosts)
                        .await?;
                Ok::<_, anyhow::Error>((vault, hosts, keyring_key.unwrap()))
            }
            .await;
            match opened {
                Ok((vault, hosts, key)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.attach_vault(vault, hosts, Some(key), cx);
                    });
                }
                Err(error) => {
                    log::warn!("加密凭据库不可用: {error}");
                    let _ = this.update(cx, |this, cx| {
                        if crate::vault::read_unlock_marker(&this.mode)
                            == crate::vault::UnlockBackend::Passphrase
                        {
                            // 口令档打不开：交回解锁弹窗处理，不降级 JSON。
                            cx.notify();
                        } else {
                            this.status = SharedString::from(format!(
                                "加密凭据库不可用（{error}），沿用旧存储且不保存密码"
                            ));
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
    }

    /// 开库成功后的统一接管：以库内列表为准接管界面主机、补发启动期间新增项、
    /// 标记 JSON 迁移、应用库内主题。
    ///
    /// `db_hosts` 必须来自凭据库本身——只靠旧 JSON 的界面列表会在数据入库后
    /// 显示为空（设置主密码后"主机消失"即由此而来，数据其实没丢）。
    fn attach_vault(
        &mut self,
        vault: Vault,
        mut db_hosts: Vec<HostConfig>,
        key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let vault = Arc::new(vault);
        self.unlock = None;
        // 开库完成前用户新增的主机（id==0，彼时尚无法入库）：补发 id 并入库。
        let mut next = db_hosts.iter().map(|host| host.id).max().unwrap_or(0) + 1;
        for host in self.hosts.iter().filter(|host| host.id == 0) {
            let mut host = host.clone();
            host.id = next;
            next += 1;
            db_hosts.push(host.clone());
            let vault = vault.clone();
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.upsert(&host).await {
                    log::warn!("凭据库写入失败: {error}");
                }
            })
            .detach();
        }
        self.store.mark_migrated();
        self.hosts = db_hosts;
        self.vault = Some(vault.clone());
        self.unlocked_key = key;
        cx.notify();
        // 库内主题优先于启动时的旧 JSON/默认值。
        cx.spawn(async move |this, cx| {
            if let Ok(Some(theme)) = vault.setting("theme").await {
                let dark = theme != "light";
                let _ = this.update(cx, |_, cx| {
                    rgpui::theme::Theme::change(
                        if dark {
                            rgpui::theme::ThemeMode::Dark
                        } else {
                            rgpui::theme::ThemeMode::Light
                        },
                        None,
                        cx,
                    );
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 打开解锁弹窗（启动自动弹出；"退出应用"关闭后可经侧边栏 🔑 重新打开）。
    fn open_unlock_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.unlock.is_some() {
            return;
        }
        let passphrase = cx.new(|cx| InputState::new(window, cx).placeholder("主密码"));
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder("再次输入确认"));
        // 回车即解锁：两个输入框都订阅提交事件，无需再点按钮。
        for input in [&passphrase, &confirm] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.unlock_passphrase(cx);
                }
            })
            .detach();
        }
        passphrase.update(cx, |state, cx| state.focus(window, cx));
        self.unlock = Some(UnlockState {
            passphrase,
            confirm,
            is_new: !self.mode.db_path().exists(),
            error: None,
        });
        cx.notify();
    }

    /// 打开口令档加密库并完成一次性迁移（解锁弹窗确认后调用）。
    fn unlock_passphrase(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.unlock.as_ref() else {
            return;
        };
        let mut ctx = &mut *cx;
        let password = state.passphrase.read(&mut ctx).text().to_string();
        if state.is_new {
            let confirm = state.confirm.read(&mut ctx).text().to_string();
            if confirm != password {
                if let Some(state) = self.unlock.as_mut() {
                    state.error = Some("两次输入的主密码不一致".into());
                }
                cx.notify();
                return;
            }
        }
        if password.is_empty() {
            if let Some(state) = self.unlock.as_mut() {
                state.error = Some("请输入主密码".into());
            }
            cx.notify();
            return;
        }
        let mode = self.mode.clone();
        let json_hosts = self.hosts.clone();
        let salt_path = mode.salt_path();
        let is_new = self.unlock.as_ref().map(|s| s.is_new).unwrap_or(false);
        let runtime = rgpui::tokio::Tokio::handle(cx);
        cx.spawn(async move |this, cx| {
            let opened = async {
                // 盐缺失时生成并写盘（首次建库）；已有盐沿用（解锁）。
                let salt = crate::vault::load_or_create_salt(&salt_path)?;
                let hexkey = crate::vault::derive_key(&salt, &password)?;
                let vault = Vault::open(&mode, hexkey.clone(), runtime.clone()).await?;
                let keyring_key = crate::vault::load_or_create_master_key().ok();
                let hosts =
                    Self::after_vault_open(&vault, &mode, keyring_key.as_deref(), &json_hosts)
                        .await?;
                if is_new {
                    crate::vault::write_unlock_marker(
                        &mode,
                        crate::vault::UnlockBackend::Passphrase,
                    )?;
                }
                Ok::<_, anyhow::Error>((vault, hosts, hexkey))
            }
            .await;
            let _ = this.update(cx, |this, cx| match opened {
                Ok((vault, hosts, hexkey)) => {
                    this.attach_vault(vault, hosts, Some(hexkey), cx);
                }
                Err(error) => {
                    if let Some(state) = this.unlock.as_mut() {
                        state.error = Some(if is_new {
                            format!("创建加密库失败：{error}").into()
                        } else {
                            "主密码不正确，请重试（多次输入错误可退出）".into()
                        });
                        log::warn!("解锁加密库失败: {error}");
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 退出解锁弹窗（尚未解锁时保持降级 JSON 行为）。
    fn dismiss_unlock(&mut self, cx: &mut Context<Self>) {
        self.unlock = None;
        self.status = "凭据库未解锁，本次运行不保存密码".into();
        cx.notify();
    }

    /// 打开设置弹窗：主密码管理与主题偏好（主密码相关操作集中在设置里，
    /// 侧边栏只留入口）。
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_at(SETTINGS_TAB_VAULT, window, cx);
    }

    /// 打开设置弹窗并直接停在指定分页（标题栏菜单按入口意图分页）。
    fn open_settings_at(&mut self, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.settings.as_mut() {
            // 已开着：只换分页（搬迁途中锁住分页，避免切走看不到结果）。
            if !state.busy {
                state.tab = tab;
                cx.notify();
            }
            return;
        }
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        // 已是口令档且处于解锁状态：换密钥必须先证明知道当前主密码。
        let needs_current = passphrase_mode && self.vault.is_some();
        let current = needs_current
            .then(|| cx.new(|cx| InputState::new(window, cx).placeholder("当前主密码")));
        let password = cx.new(|cx| {
            InputState::new(window, cx).placeholder(password_placeholder(passphrase_mode))
        });
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder("再次输入确认"));
        // 任一输入框回车即保存，不必再点按钮。
        for input in [&password, &confirm].into_iter().chain(current.iter()) {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.apply_rekey(cx);
                }
            })
            .detach();
        }
        current
            .as_ref()
            .unwrap_or(&password)
            .update(cx, |state, cx| state.focus(window, cx));
        self.settings = Some(SettingsState {
            tab,
            current,
            password: Some(password),
            confirm: Some(confirm),
            error: None,
            info: None,
            busy: false,
        });
        cx.notify();
    }

    /// 切换设置弹窗分页；主密码搬迁途中锁住分页，免得用户切走看不到结果。
    fn select_settings_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(state) = self.settings.as_mut() else {
            return;
        };
        if state.busy || state.tab == ix {
            return;
        }
        state.tab = ix;
        cx.notify();
    }

    /// 重建口令表单（成功回显后输入框已销毁，"更改主密码"再次开启）。
    fn reopen_password_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .settings
            .as_ref()
            .is_some_and(|state| state.password.is_some())
        {
            return;
        }
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        let current = passphrase_mode
            .then(|| cx.new(|cx| InputState::new(window, cx).placeholder("当前主密码")));
        let password = cx.new(|cx| {
            InputState::new(window, cx).placeholder(password_placeholder(passphrase_mode))
        });
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder("再次输入确认"));
        for input in [&password, &confirm].into_iter().chain(current.iter()) {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.apply_rekey(cx);
                }
            })
            .detach();
        }
        let focus = current.as_ref().unwrap_or(&password).clone();
        if let Some(state) = self.settings.as_mut() {
            state.error = None;
            state.info = None;
            state.current = current;
            state.password = Some(password);
            state.confirm = Some(confirm);
        }
        focus.update(cx, |field, cx| field.focus(window, cx));
        cx.notify();
    }

    /// 关闭设置弹窗（搬迁进行中不许关闭，避免半途中止留下半套文件）。
    fn close_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.as_ref().is_some_and(|state| state.busy) {
            return;
        }
        self.settings = None;
        cx.notify();
    }

    /// 锁定凭据库：丢弃库句柄与内存中的主密钥，清空主机列表并回到解锁弹窗。
    /// 已建立的终端会话不受影响，但不再能读取保存的凭据。
    fn lock_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.vault = None;
        self.unlocked_key = None;
        self.password_cache.clear();
        self.hosts.clear();
        self.settings = None;
        self.status = "凭据库已锁定，需主密码解锁".into();
        self.open_unlock_dialog(window, cx);
        cx.notify();
    }

    /// 点击"保存主密码"或回车：设置 / 更换主密码。
    fn apply_rekey(&mut self, cx: &mut Context<Self>) {
        self.submit_master_password(MasterPasswordAction::Set, cx);
    }

    /// 点击"取消主密码"：库密钥换回系统凭据管理器里的随机密钥，此后启动无需输入口令。
    fn cancel_master_password(&mut self, cx: &mut Context<Self>) {
        self.submit_master_password(MasterPasswordAction::Cancel, cx);
    }

    /// 执行主密码设置 / 更换 / 取消：校验当前口令 → 求目标密钥（新口令派生或 keyring）
    /// → 建新库全量搬迁 → 旧库让位 → 新库就位 → 写盐与解锁标记 → 确认数据无损后清理备份。
    fn submit_master_password(&mut self, action: MasterPasswordAction, cx: &mut Context<Self>) {
        let Some(state) = self.settings.as_ref() else {
            return;
        };
        if state.busy {
            return;
        }
        let mut ctx = &mut *cx;
        let needs_current = state.current.is_some();
        let current = state
            .current
            .as_ref()
            .map(|input| input.read(&mut ctx).text().to_string());
        let password = state
            .password
            .as_ref()
            .map(|input| input.read(&mut ctx).text().to_string());
        let confirm = state
            .confirm
            .as_ref()
            .map(|input| input.read(&mut ctx).text().to_string());
        let busy = state.busy;
        if busy {
            return;
        }
        let fail = |this: &mut Self, message: SharedString, cx: &mut Context<Self>| {
            if let Some(state) = this.settings.as_mut() {
                state.error = Some(message);
            }
            cx.notify();
        };
        // 口令档免验证换钥等于没锁：应用可能在托盘里挂上几个小时。
        if needs_current {
            let verified = match (&self.unlocked_key, current.as_deref()) {
                (Some(expected), Some(entered)) => {
                    match crate::vault::load_or_create_salt(&self.mode.salt_path()) {
                        Ok(salt) => crate::vault::derive_key(&salt, entered)
                            .map(|derived| derived == *expected)
                            .unwrap_or(false),
                        Err(_) => false,
                    }
                }
                _ => false,
            };
            if !verified {
                fail(self, "当前主密码不正确".into(), cx);
                return;
            }
        }
        // 目标密钥与新盐：盐先只生成不落盘，等新库就位后才写入，
        // 否则搬迁失败会让旧口令再也派生不出旧密钥。
        let (backend, hexkey, new_salt) = match action {
            MasterPasswordAction::Set => {
                let (password, confirm) = match (&password, &confirm) {
                    (Some(password), Some(confirm)) => (password.clone(), confirm.clone()),
                    // 表单已销毁（上一次提交成功后回收）：拒绝空提交。
                    _ => {
                        fail(self, "请重新填写主密码".into(), cx);
                        return;
                    }
                };
                if password.chars().count() < 8 {
                    fail(self, "主密码至少 8 位".into(), cx);
                    return;
                }
                if password != confirm {
                    fail(self, "两次输入不一致".into(), cx);
                    return;
                }
                let salt = match crate::vault::generate_salt() {
                    Ok(salt) => salt,
                    Err(error) => {
                        fail(self, SharedString::from(error.to_string()), cx);
                        return;
                    }
                };
                let hexkey = match crate::vault::derive_key(&salt, &password) {
                    Ok(hexkey) => hexkey,
                    Err(error) => {
                        fail(self, SharedString::from(error.to_string()), cx);
                        return;
                    }
                };
                (crate::vault::UnlockBackend::Passphrase, hexkey, Some(salt))
            }
            MasterPasswordAction::Cancel => match crate::vault::load_or_create_master_key() {
                Ok(hexkey) => (crate::vault::UnlockBackend::Keyring, hexkey, None),
                Err(error) => {
                    fail(
                        self,
                        SharedString::from(format!("系统凭据管理器不可用：{error}")),
                        cx,
                    );
                    return;
                }
            },
        };
        let Some(vault) = self.vault.take() else {
            self.status = "加密库尚未就绪，无法更改主密码".into();
            cx.notify();
            return;
        };
        let mode = self.mode.clone();
        // 换名需要旧库文件句柄已释放：句柄交给本任务独占，搬迁后即 drop。
        let runtime = vault.runtime();
        if let Some(state) = self.settings.as_mut() {
            state.busy = true;
            state.error = None;
            state.info = None;
        }
        self.status = "正在重建加密库…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = async {
                let dump = vault.export_all().await?;
                let expected_hosts = dump.hosts.len();
                let salt_path = mode.salt_path();
                let previous_salt = std::fs::read(&salt_path).ok();
                // turso 无 rekey：新建加密库全量搬迁，再原子换名。
                let staging = mode.db_path().with_file_name("rulink.db.new");
                // 上一轮失败可能留下带旧数据的暂存束（含 -wal），必须先整套清掉。
                crate::vault::remove_db_bundle(&staging);
                std::fs::create_dir_all(mode.dir())?;
                let staging_db = staging.to_string_lossy().to_string();
                let fresh = Vault::open_at(&staging_db, hexkey.clone(), runtime.clone()).await?;
                fresh.import_dump(&dump).await?;
                drop(fresh);
                drop(vault);
                let db_path = mode.db_path();
                let mut backup: Option<std::path::PathBuf> = None;
                if db_path.exists() {
                    let candidate = db_path.with_file_name("rulink.db.keyring-bak");
                    let candidate = if candidate.exists() {
                        // 备份名被占用时带时间戳另存，确保旧库不丢。
                        db_path.with_file_name(format!(
                            "rulink.db.bak-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0)
                        ))
                    } else {
                        candidate
                    };
                    crate::vault::rename_db_bundle(
                        &runtime,
                        db_path.clone(),
                        candidate.clone(),
                        100,
                    )
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("旧库让位失败（数据原样保留，可直接重试）: {error}")
                    })?;
                    // 新库就位失败时把旧库放回原处，绝不留下"两头空"。
                    if let Err(error) = crate::vault::rename_db_bundle(
                        &runtime,
                        staging.clone(),
                        db_path.clone(),
                        100,
                    )
                    .await
                    {
                        let _ = crate::vault::rename_db_bundle(
                            &runtime,
                            candidate,
                            db_path.clone(),
                            100,
                        )
                        .await;
                        return Err(anyhow::anyhow!("新库就位失败（旧库已放回原处）: {error}"));
                    }
                    backup = Some(candidate);
                } else {
                    crate::vault::rename_db_bundle(&runtime, staging, db_path.clone(), 100)
                        .await
                        .map_err(anyhow::Error::msg)?;
                }
                // 新库已就位：此时才可以换盐、换标记。
                if let Some(salt) = new_salt {
                    if let Err(error) = crate::vault::write_salt(&salt_path, &salt) {
                        if let Some(previous) = previous_salt.as_deref() {
                            let _ = std::fs::write(&salt_path, previous);
                        }
                        return Err(anyhow::anyhow!("写入新派生盐失败（已还原旧盐）: {error}"));
                    }
                }
                crate::vault::write_unlock_marker(&mode, backend)?;
                let reopened = Vault::open(&mode, hexkey.clone(), runtime.clone()).await?;
                let hosts = reopened.load_hosts().await?;
                // 只有确认新库数据完整（主机数不少于导出数）才清理旧库备份，
                // 否则保留备份留待人工处置。
                if hosts.len() >= expected_hosts {
                    if let Some(path) = backup.take() {
                        crate::vault::remove_db_bundle(&path);
                        log::info!("凭据库重建完成，旧库备份已清理");
                    }
                } else {
                    log::warn!(
                        "新库主机数 {} 少于导出的 {}，保留旧库备份 {}",
                        hosts.len(),
                        expected_hosts,
                        backup
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    );
                }
                Ok::<_, anyhow::Error>((reopened, hosts, hexkey, backup))
            }
            .await;
            let done = match action {
                MasterPasswordAction::Set => "主密码已生效，下次启动需用其解锁",
                MasterPasswordAction::Cancel => {
                    "已取消主密码，改用本机凭据管理器免密解锁（库文件因此绑定本机）"
                }
            };
            let _ = this.update(cx, |this, cx| match result {
                Ok((vault, hosts, hexkey, kept_backup)) => {
                    this.vault = Some(Arc::new(vault));
                    this.hosts = hosts;
                    this.unlocked_key = Some(hexkey);
                    if let Some(state) = this.settings.as_mut() {
                        state.busy = false;
                        state.error = None;
                        // 输入框销毁：明文口令随之从界面缓冲消失，需再改时重新开启表单。
                        state.current = None;
                        state.password = None;
                        state.confirm = None;
                        state.info = Some(match kept_backup {
                            None => SharedString::from(format!("{done}；旧库备份已清理，数据完整")),
                            Some(path) => SharedString::from(format!(
                                "{done}，但新库数据不完整，旧库备份保留在 {}",
                                path.display()
                            )),
                        });
                    }
                    this.status = done.into();
                    cx.notify();
                }
                Err(error) => {
                    log::warn!("更改主密码失败: {error}");
                    if let Some(state) = this.settings.as_mut() {
                        state.busy = false;
                        state.error = Some(format!("更改失败：{error}").into());
                    }
                    this.restore_vault(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 换密钥失败后尽力恢复库句柄：keyring 档直接重开；口令档旧密钥只存在于
    /// 解锁输入中，交回设置里的解锁入口。
    fn restore_vault(&mut self, cx: &mut Context<Self>) {
        if self.vault.is_some() {
            return;
        }
        if crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase {
            self.status = "凭据库未就绪，点击侧边栏 🔑 重新解锁".into();
            return;
        }
        let mode = self.mode.clone();
        let runtime = rgpui::tokio::Tokio::handle(cx);
        cx.spawn(async move |this, cx| {
            let Ok(key) = crate::vault::load_or_create_master_key() else {
                return;
            };
            if let Ok(vault) = Vault::open(&mode, key.clone(), runtime).await {
                let _ = this.update(cx, |this, cx| {
                    this.vault = Some(Arc::new(vault));
                    this.unlocked_key = Some(key);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 一次性迁移 + JSON 导入（boot 与解锁共用）。`keyring_hexkey` 用于读取旧 vault.db。
    /// 返回库内当前主机列表，交由调用方接管界面。
    async fn after_vault_open(
        vault: &Vault,
        mode: &StorageMode,
        keyring_hexkey: Option<&str>,
        json_hosts: &[HostConfig],
    ) -> anyhow::Result<Vec<HostConfig>> {
        if let Some(key) = keyring_hexkey {
            if let Err(error) = crate::vault::migrate_legacy(vault, mode, key).await {
                log::warn!("旧数据迁移未完成: {error}");
            }
        }
        let existing = vault.load_hosts().await?;
        if existing.is_empty() && !json_hosts.is_empty() {
            let mut imported = json_hosts.to_vec();
            for (index, host) in imported.iter_mut().enumerate() {
                host.id = index as i64 + 1;
            }
            vault.import_hosts(&imported).await?;
            return Ok(imported);
        }
        Ok(existing)
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
        } = session::start(target, handles, self.vault.clone(), cx);
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

    /// 切换明暗主题：翻转当前模式后落到 `apply_theme`。
    fn toggle_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = cx.theme().is_dark();
        self.apply_theme(!dark, window, cx);
    }

    /// 设定明暗主题：翻转全局 Theme 模式并持久化，界面（含状态栏/按钮）随之重取配色。
    fn apply_theme(&self, dark: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if dark {
            rgpui::theme::ThemeMode::Dark
        } else {
            rgpui::theme::ThemeMode::Light
        };
        rgpui::theme::Theme::change(mode, Some(window), cx);
        if let Some(vault) = self.vault.clone() {
            let tag = if dark { "dark" } else { "light" };
            let tag = SharedString::from(tag);
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.set_setting("theme", &tag).await {
                    log::warn!("写入库内主题失败: {error}");
                }
            })
            .detach();
        } else {
            // 降级模式（无加密库）沿用旧 settings.json。
            self.store.save_theme(dark);
        }
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
            .child(self.render_title_bar(window, cx))
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
    /// 标题栏右侧的窗口控制按钮：登记为 `WindowControlArea`，交给系统
    /// HTMINBUTTON / HTMAXBUTTON / HTCLOSE（关闭仍会走 `on_window_should_close`，
    /// 即隐藏到托盘）。
    #[allow(clippy::too_many_arguments)]
    fn window_control(
        id: &'static str,
        icon: IconName,
        area: WindowControlArea,
        fg: Hsla,
        hover_bg: Hsla,
        active_bg: Hsla,
        hover_fg: Hsla,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .w(TITLE_BAR_HEIGHT)
            .h_full()
            .flex_shrink_0()
            .justify_center()
            .content_center()
            .items_center()
            .text_color(fg)
            .hover(|style| style.bg(hover_bg).text_color(hover_fg))
            .active(|style| style.bg(active_bg).text_color(hover_fg))
            .window_control_area(area)
            .child(Icon::new(icon).small())
            .into_any_element()
    }

    /// 自定义标题栏：品牌 + 菜单（会话 / 凭据 / 外观 / 帮助）+ 快捷按钮 + 窗口控制。
    ///
    /// 这里不套框架的 `TitleBar`：它把 children 整体放进 `WindowControlArea::Drag`
    /// 命中区，而 Windows 的 WM_NCHITTEST 只取**第一个**命中的控制区，父级 Drag 先入表，
    /// 栏内按钮一律被判定为 caption，点击被系统吃掉（hover 仍在、click 永不触发）。
    /// 改成把拖拽区拆成"品牌段"与"中间空档"两个兄弟节点，交互控件落在 Drag 区之外。
    fn render_title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let bar_bg = cx.theme().tokens.title_bar;
        let bar_border = cx.theme().title_bar_border;
        let fg = cx.theme().foreground;
        let plain_hover_bg = cx.theme().secondary_hover;
        let plain_active_bg = cx.theme().secondary_active;
        let plain_hover_fg = cx.theme().secondary_foreground;
        let close_hover_bg = cx.theme().danger;
        let close_active_bg = cx.theme().danger_active;
        let close_hover_fg = cx.theme().danger_foreground;
        let dark = cx.theme().is_dark();
        let maximized = window.is_maximized();
        // 只有口令档（已设置主密码）才有"锁定/解锁"概念，keyring 档免密无锁可上。
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        let locked = passphrase_mode && self.vault.is_none();
        let vault_state: &'static str = if !passphrase_mode {
            "免主密码（系统密钥环）"
        } else if locked {
            "主密码档 · 已锁定"
        } else {
            "主密码档 · 已解锁"
        };
        let entity = cx.entity();

        // 会话菜单：新建主机、侧栏折叠、托盘与退出。
        let session = entity.clone();
        let session2 = entity.clone();
        let session_menu = move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
            // 菜单构建器要求 `Fn`（可重复调用），每次重建时重新克隆实体句柄交给各项回调。
            let session = session.clone();
            let session2 = session2.clone();
            menu.item(
                PopupMenuItem::new("新建主机…").on_click(move |_, window, cx| {
                    session.update(cx, |this, cx| this.open_form(window, cx));
                }),
            )
            .item(
                PopupMenuItem::new("折叠 / 展开侧边栏 (Ctrl+B)").on_click(move |_, _, cx| {
                    session2.update(cx, |this, cx| {
                        this.split_state.update(cx, |state, cx| {
                            state.toggle_collapse(CollapsiblePane::First, cx);
                        });
                        cx.notify();
                    });
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new("隐藏到托盘").on_click(move |_, window, _| {
                    window.hide_window();
                }),
            )
            .item(
                PopupMenuItem::new("退出 RuLink")
                    .danger(true)
                    .on_click(move |_, _, cx| cx.quit()),
            )
        };

        // 凭据菜单：解锁 / 主密码设置 / 锁定，末尾附当前档位（只读）。
        let unlock = entity.clone();
        let settings = entity.clone();
        let lock = entity.clone();
        let vault_menu = move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
            let unlock = unlock.clone();
            let settings = settings.clone();
            let lock = lock.clone();
            let menu = if locked {
                menu.item(
                    PopupMenuItem::new("解锁凭据库…").on_click(move |_, window, cx| {
                        unlock.update(cx, |this, cx| this.open_unlock_dialog(window, cx));
                    }),
                )
            } else {
                menu.item(PopupMenuItem::new("主密码与凭据库设置…").on_click(
                    move |_, window, cx| {
                        settings.update(cx, |this, cx| {
                            this.open_settings_at(SETTINGS_TAB_VAULT, window, cx);
                        });
                    },
                ))
            };
            let menu = if passphrase_mode && !locked {
                menu.item(
                    PopupMenuItem::new("锁定凭据库").on_click(move |_, window, cx| {
                        lock.update(cx, |this, cx| this.lock_vault(window, cx));
                    }),
                )
            } else {
                menu
            };
            menu.separator()
                .item(PopupMenuItem::new(vault_state).disabled(true))
        };

        // 外观菜单：明暗二选一（当前项打勾），外加设置面板的外观分页。
        let to_dark = entity.clone();
        let to_light = entity.clone();
        let appearance = entity.clone();
        let appearance_menu =
            move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
                let to_dark = to_dark.clone();
                let to_light = to_light.clone();
                let appearance = appearance.clone();
                menu.item(PopupMenuItem::new("暗色主题").checked(dark).on_click(
                    move |_, window, cx| {
                        to_dark.update(cx, |this, cx| this.apply_theme(true, window, cx));
                    },
                ))
                .item(PopupMenuItem::new("亮色主题").checked(!dark).on_click(
                    move |_, window, cx| {
                        to_light.update(cx, |this, cx| this.apply_theme(false, window, cx));
                    },
                ))
                .separator()
                .item(
                    PopupMenuItem::new("外观设置…").on_click(move |_, window, cx| {
                        appearance.update(cx, |this, cx| {
                            this.open_settings_at(SETTINGS_TAB_APPEARANCE, window, cx);
                        });
                    }),
                )
            };

        let help_menu = move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
            menu.label("RuLink")
                .item(
                    PopupMenuItem::new(concat!("版本 ", env!("CARGO_PKG_VERSION"))).disabled(true),
                )
                .separator()
                .item(PopupMenuItem::new("Ctrl+B 折叠 / 展开侧边栏").disabled(true))
                .item(PopupMenuItem::new("Ctrl+T 切换明暗主题").disabled(true))
        };

        let menu_button = |id: &'static str, label: &'static str| {
            Button::new(id)
                .label(label)
                .compact()
                .ghost()
                .text_color(fg)
        };

        h_flex()
            .id("title-bar")
            .flex_shrink_0()
            .w_full()
            .h(TITLE_BAR_HEIGHT)
            .items_center()
            .pl(px(12.0))
            .border_b_1()
            .border_color(bar_border)
            .bg(bar_bg)
            .text_color(fg)
            // 可拖动段一：品牌与锁定提示。
            .child(
                h_flex()
                    .id("titlebar-brand")
                    .h_full()
                    .items_center()
                    .gap_2()
                    .pr_3()
                    .flex_shrink_0()
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight(700.0))
                            .child("RuLink"),
                    )
                    .when(locked, |this| {
                        this.child(div().text_sm().text_color(pal.muted).child("已锁定"))
                    }),
            )
            // 菜单区：交互控件，必须落在 Drag 命中区之外。
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .child(menu_button("menu-session", "会话").dropdown_menu(session_menu))
                    .child(menu_button("menu-vault", "凭据").dropdown_menu(vault_menu))
                    .child(menu_button("menu-appearance", "外观").dropdown_menu(appearance_menu))
                    .child(menu_button("menu-help", "帮助").dropdown_menu(help_menu)),
            )
            // 可拖动段二：菜单与右侧按钮之间的空档。
            .child(
                div()
                    .id("titlebar-gap")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .pr_1()
                    // 口令档且未解锁：只留重新解锁入口（其余操作都需要库）。
                    .when(locked, |this| {
                        this.child(
                            Button::new("unlock-vault")
                                .label("🔑")
                                .compact()
                                .ghost()
                                .tooltip("输入主密码解锁凭据库")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_unlock_dialog(window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("open-settings")
                            .label("⚙")
                            .compact()
                            .ghost()
                            .tooltip("设置（凭据库 / 外观分页）")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings(window, cx);
                            })),
                    )
                    // 已设主密码才出现锁定按钮：锁定后需重新输入口令才能用库。
                    .when(passphrase_mode && !locked, |this| {
                        this.child(
                            Button::new("lock-vault")
                                .label("🔒")
                                .compact()
                                .ghost()
                                .tooltip("锁定凭据库（需主密码解锁）")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.lock_vault(window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("sidebar-collapse")
                            .label("«")
                            .compact()
                            .ghost()
                            .tooltip("折叠侧边栏 (Ctrl+B)")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.split_state.update(cx, |state, cx| {
                                    state.collapse(CollapsiblePane::First, cx);
                                });
                                cx.notify();
                            })),
                    ),
            )
            .child(Self::window_control(
                "win-minimize",
                IconName::WindowMinimize,
                WindowControlArea::Min,
                fg,
                plain_hover_bg,
                plain_active_bg,
                plain_hover_fg,
            ))
            .child(Self::window_control(
                "win-maximize",
                if maximized {
                    IconName::WindowRestore
                } else {
                    IconName::WindowMaximize
                },
                WindowControlArea::Max,
                fg,
                plain_hover_bg,
                plain_active_bg,
                plain_hover_fg,
            ))
            .child(Self::window_control(
                "win-close",
                IconName::WindowClose,
                WindowControlArea::Close,
                fg,
                close_hover_bg,
                close_active_bg,
                close_hover_fg,
            ))
            .into_any_element()
    }

    /// 左侧栏：新建按钮 / 搜索框 / 主机列表面板（品牌与全局入口已移至标题栏）。
    /// 宽度与折叠由外层 SplitPane 负责，这里只填充第一面板区域。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let panel = if self.pending.is_some() {
            self.render_pending_panel(cx)
        } else {
            self.render_host_list(cx)
        };

        // 搜索框只在主机列表模式下出现（待输入密码时隐藏；表单已是居中弹窗，不占列表）。
        let list_mode = self.pending.is_none();

        v_flex()
            .size_full()
            .bg(pal.sidebar_bg)
            .p_3()
            .gap_2()
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

    /// 便携/主密码库的解锁（或首次建库）居中弹窗。
    fn render_unlock_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.unlock.as_ref() else {
            return div().into_any_element();
        };
        let is_new = state.is_new;
        v_flex()
            .id("unlock-backdrop")
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
                    .id("unlock-dialog")
                    .w(px(420.0))
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
                        div()
                            .text_base()
                            .font_weight(rgpui::FontWeight(600.0))
                            .child(if is_new {
                                "设置主密码"
                            } else {
                                "解锁凭据库"
                            }),
                    )
                    .child(div().text_sm().text_color(pal.muted).child(if is_new {
                        "便携模式首次使用：设置主密码以创建加密库。密码仅用于派生库密钥，\
                                 不会保存到任何地方——一旦忘记无法恢复，请务必牢记。"
                    } else {
                        "输入主密码解锁凭据库（库文件与密码均不离开本机/本目录）。"
                    }))
                    .child(styled_input(
                        Input::new(&state.passphrase).content_type(InputContentType::Password),
                        &pal,
                    ))
                    .when(is_new, |this| {
                        this.child(styled_input(
                            Input::new(&state.confirm).content_type(InputContentType::Password),
                            &pal,
                        ))
                    })
                    .when_some(state.error.as_ref().cloned(), |this, error| {
                        this.child(div().text_sm().text_color(pal.danger).child(error))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("unlock-cancel")
                                    .label("退出应用")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        // 未解锁即退出：保持旧行为，不落任何秘密。
                                        {
                                            let cx = &mut *cx;
                                            this.dismiss_unlock(cx);
                                        }
                                        cx.quit();
                                    })),
                            )
                            .child(
                                Button::new("unlock-confirm")
                                    .label(if is_new { "创建并解锁" } else { "解锁" })
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.unlock_passphrase(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 设置居中弹窗：分页展示「凭据库与主密码」与「外观」。
    fn render_settings_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.settings.as_ref() else {
            return div().into_any_element();
        };
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        let unlocked = self.vault.is_some();
        let dark = cx.theme().is_dark();
        let body = if state.tab == SETTINGS_TAB_APPEARANCE {
            self.render_appearance_tab(&pal, dark, cx)
        } else {
            self.render_vault_tab(&pal, state, passphrase_mode, unlocked, cx)
        };
        v_flex()
            .id("settings-backdrop")
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
                    .id("settings-dialog")
                    .w(px(520.0))
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
                        div()
                            .text_base()
                            .font_weight(FontWeight(600.0))
                            .child("设置"),
                    )
                    .child(
                        TabBar::new("settings-tabs")
                            .underline()
                            .children(vec![
                                Tab::new().label("凭据库与主密码"),
                                Tab::new().label("外观"),
                            ])
                            .selected_index(state.tab)
                            .on_change({
                                let entity = cx.entity();
                                move |ix, _window, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.select_settings_tab(ix, cx);
                                    });
                                }
                            }),
                    )
                    .child(body)
                    .child(
                        h_flex().gap_2().justify_end().child(
                            Button::new("settings-close")
                                .label("关闭")
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_settings(cx);
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// 「凭据库与主密码」分页：库位置与解锁方式、主密码设置/更换/取消、一键锁定。
    fn render_vault_tab(
        &self,
        pal: &Palette,
        state: &SettingsState,
        passphrase_mode: bool,
        unlocked: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        v_flex()
            .id("settings-tab-vault")
            .gap_3()
            .child(div().text_sm().text_color(pal.muted).child(format!(
                "位置：{}　解锁方式：{}",
                self.mode.dir().display(),
                if passphrase_mode {
                    "主密码（Argon2id 派生）"
                } else {
                    "系统凭据管理器（无需输入）"
                }
            )))
            .child(
                div()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(if passphrase_mode {
                        "主密码只用于派生库密钥，不会写入任何文件；忘记主密码将无法恢复数据。\
                         每次启动需输入主密码，库文件可凭口令带往其他机器解锁。"
                    } else {
                        "当前由系统凭据管理器免密解锁，开箱即用。建议设置主密码：库文件可凭口令\
                         带往其他机器，他人取到文件也无法读取；忘记主密码将无法恢复数据。"
                    }),
            )
            .when(!unlocked, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(pal.danger)
                        .child("凭据库未解锁，无法设置或更换主密码。"),
                )
            })
            .when(unlocked, |this| {
                let form = state.password.as_ref().zip(state.confirm.as_ref());
                match form {
                    Some((password, confirm)) => {
                        let this =
                            this.child(div().text_base().font_weight(FontWeight(600.0)).child(
                                if passphrase_mode {
                                    "更换主密码"
                                } else {
                                    "设置主密码（可选）"
                                },
                            ));
                        let this = match state.current.as_ref() {
                            Some(current) => this.child(styled_input(
                                Input::new(current).content_type(InputContentType::Password),
                                &pal,
                            )),
                            None => this,
                        };
                        this.child(styled_input(
                            Input::new(password).content_type(InputContentType::Password),
                            &pal,
                        ))
                        .child(styled_input(
                            Input::new(confirm).content_type(InputContentType::Password),
                            &pal,
                        ))
                        .when_some(state.error.as_ref().cloned(), |this, error| {
                            this.child(div().text_sm().text_color(pal.danger).child(error))
                        })
                        .when(state.busy, |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(pal.muted)
                                    .child("正在新建加密库并全量搬迁，请稍候…"),
                            )
                        })
                        .when(!state.busy, |this| {
                            this.child(
                                h_flex()
                                    .gap_2()
                                    .justify_end()
                                    .when(passphrase_mode, |this| {
                                        this.child(
                                            Button::new("rekey-cancel")
                                                .label("取消主密码")
                                                .ghost()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cancel_master_password(cx);
                                                })),
                                        )
                                    })
                                    .child(
                                        Button::new("rekey-apply")
                                            .label(if passphrase_mode {
                                                "更换主密码"
                                            } else {
                                                "设置主密码"
                                            })
                                            .primary()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.apply_rekey(cx);
                                            })),
                                    ),
                            )
                        })
                    }
                    // 提交成功后表单已销毁（明文口令不留界面），只留结果与重开入口。
                    None => this
                        .when_some(state.info.as_ref().cloned(), |this, info| {
                            this.child(div().text_sm().text_color(pal.accent).child(info))
                        })
                        .when_some(state.error.as_ref().cloned(), |this, error| {
                            this.child(div().text_sm().text_color(pal.danger).child(error))
                        })
                        .when(!state.busy, |this| {
                            this.child(
                                h_flex().gap_2().justify_end().child(
                                    Button::new("rekey-reopen")
                                        .label(if passphrase_mode {
                                            "更换主密码"
                                        } else {
                                            "设置主密码"
                                        })
                                        .ghost()
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reopen_password_form(window, cx);
                                        })),
                                ),
                            )
                        }),
                }
            })
            .when(passphrase_mode && unlocked, |this| {
                this.child(
                    h_flex().gap_2().justify_end().child(
                        Button::new("settings-lock")
                            .label("锁定凭据库")
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.lock_vault(window, cx);
                            })),
                    ),
                )
            })
            .into_any_element()
    }

    /// 「外观」分页：明暗主题（与 `Ctrl+T`、标题栏 ☀☾ 同一开关）。
    fn render_appearance_tab(&self, pal: &Palette, dark: bool, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .id("settings-tab-appearance")
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(pal.muted)
                    .child("主题切换立即生效并写入凭据库，下次启动沿用。"),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(pal.muted)
                            .child(format!("当前主题：{}", if dark { "暗色" } else { "亮色" })),
                    )
                    .child(
                        Button::new("settings-toggle-theme")
                            .label(if dark {
                                "切换到亮色"
                            } else {
                                "切换到暗色"
                            })
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_theme(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// 窗口级浮层：主机右键菜单与删除二次确认框（绝对定位渲染在最上层）。
    fn render_overlay(&self, window: &mut Window, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        // 解锁 / 主密码弹窗优先于一切浮层（库未解锁时其余操作无意义）。
        if self.unlock.is_some() {
            return self.render_unlock_dialog(cx);
        }
        if self.settings.is_some() {
            return self.render_settings_dialog(cx);
        }
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
        // 新建/编辑主机：居中模态弹窗（不再挤占侧边栏窄面板）。
        if self.new_host.is_some() {
            return self.render_form_dialog(cx);
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

    /// 新建/编辑主机：居中模态弹窗。遮罩只挡交互、点击不关闭（防误丢已填内容），
    /// 关闭仅走"取消"按钮或标题栏 ×。
    fn render_form_dialog(&self, cx: &Context<Self>) -> AnyElement {
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
        let save_checked = form.save_secret;
        let editing = form.editing.is_some();
        v_flex()
            .id("form-backdrop")
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
                    .id("host-form")
                    .w(px(440.0))
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
                                    .font_weight(rgpui::FontWeight(600.0))
                                    .child(if editing {
                                        "编辑主机"
                                    } else {
                                        "新建主机"
                                    }),
                            )
                            .child(
                                Button::new("close-form")
                                    .label("×")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
                            ),
                    )
                    .child(labeled_field("展示名", &form.name, &pal))
                    .child(labeled_field("主机地址", &form.host, &pal))
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .child(labeled_field("端口", &form.port, &pal)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .child(labeled_field("用户名", &form.user, &pal)),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child("认证方式"))
                            .child(h_flex().gap_2().children(methods.into_iter().map(|method| {
                                let auth_id =
                                    SharedString::from(format!("auth-{}", auth_label(method)));
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
                    // 私钥区块常驻布局：非私钥时用 invisible 保留占位（隐藏绘制与事件），
                    // 切换认证方式弹窗高度不跳变。
                    .child(
                        v_flex()
                            .gap_3()
                            .when(selected != AuthMethod::IdentityFile, |this| {
                                this.invisible()
                            })
                            .child(
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
                                        Input::new(&form.passphrase)
                                            .content_type(InputContentType::Password),
                                        &pal,
                                    )),
                            )
                            .child(
                                h_flex()
                                    .id("save-passphrase")
                                    .gap_2()
                                    .items_center()
                                    .when(self.vault.is_some(), |this| {
                                        this.cursor_pointer().on_click(cx.listener(
                                            |this, _, _, cx| {
                                                if let Some(form) = this.new_host.as_mut() {
                                                    form.save_secret = !form.save_secret;
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                    })
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(if save_checked {
                                                pal.text
                                            } else {
                                                pal.muted
                                            })
                                            .child(if save_checked { "☑" } else { "☐" }),
                                    )
                                    .child(div().text_sm().text_color(pal.muted).child(
                                        if self.vault.is_some() {
                                            "保存口令（加密存入本地凭据库，默认仅本次内存持有）"
                                        } else {
                                            "加密库不可用，口令仅本次内存持有"
                                        },
                                    )),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
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
        // theme::init 默认切到亮色；库内主题要等开库后才能读到，这里先按
        // 旧 settings.json（降级/迁移前）或默认暗色起窗，避免闪一下亮色。
        let dark = SessionStore::new(&StorageMode::detect())
            .load_settings()
            .dark();
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
                    // 隐藏系统标题栏，改由应用自绘（品牌 + 菜单 + 窗口控制按钮）。
                    appears_transparent: true,
                    ..Default::default()
                }),
                // 客户区装饰：窗口样式改为 WS_THICKFRAME 无边框（DWM 仍画圆角与可拖边框），
                // 拖拽与窗口控制由标题栏里显式登记的 WindowControlArea 命中区交给系统。
                app_owns_titlebar_drag: true,
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
