//! //! 主机列表：分组树 / 排序 / 收藏 / 删除 / 导入 ssh config，主机行与首页卡片渲染。

use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, ContextMenuExt, Div, Entity, FontWeight, Icon,
    IconName, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels,
    ScrollableElement, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div,
    h_flex, px, v_flex,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;

use crate::desktop::Protocol;
use crate::sessions::{AuthMethod, HostConfig, ProtocolKind};
use crate::tr;
use crate::{NewHost, NewLocalTerminal};

use super::*;

/// 分组新建 / 重命名弹窗状态。
#[derive(Clone)]
pub(super) struct GroupPrompt {
    /// Some(原名)＝重命名既有分组；None＝新建。
    pub(super) renaming: Option<SharedString>,
    pub(super) name: Entity<InputState>,
}

/// 「会话」菜单里最近连接最多保留几条；再多菜单就太长了。
pub(super) const RECENT_HOSTS_MAX: usize = 8;

#[derive(Serialize, Deserialize)]
struct HostConfigBackup {
    version: u32,
    hosts: Vec<HostConfig>,
    #[serde(default)]
    sidebar: SidebarPrefs,
}

/// 侧边栏右键菜单的目标。
#[derive(Clone)]
pub(super) enum MenuTarget {
    /// 列表空白处：只给新建类入口。
    Blank,
    /// 某个分组（文件夹）：附带重命名 / 删除。
    Group(SharedString),
    /// 某台主机行（凭据库主键）：附带编辑、移动分组、删除。
    /// 菜单闭包的生命周期可能横跨一次后台 `attach_vault` 整表替换，
    /// 裸下标在替换后会指向另一台主机——身份必须用 id，动作时再解析。
    HostId(i64),
}

/// 组内手动排序动作（收藏 / 非收藏各自成桶，不互相穿插）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReorderAction {
    Up,
    Down,
    Top,
}

impl RuTermApp {
    /// 持久化一台主机：加密库可用则只写该行，否则整表回写旧 JSON。
    pub(super) fn persist_host(&mut self, host: &HostConfig, cx: &mut Context<Self>) {
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

    /// 从 `~/.ssh/config` 导入具体 Host 条目。
    ///
    /// 通配符 / `Match` 块跳过；按 user+host+port 或同名去重；
    /// 私钥文件在本机不存在时改用 Agent（避免导入后在加载私钥这一步直接失败）。
    pub(super) fn import_ssh_config(&mut self, cx: &mut Context<Self>) {
        let path = crate::ssh_config::config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                self.status = SharedString::from(format!("读取 {} 失败：{error}", path.display()));
                cx.notify();
                return;
            }
        };
        let entries = crate::ssh_config::parse(&text);
        if entries.is_empty() {
            self.status = SharedString::from(format!(
                "{} 里没有可导入的具体主机（通配符与 Match 已跳过）",
                path.display()
            ));
            cx.notify();
            return;
        }
        let fallback_user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "root".to_string());
        let mut imported = 0usize;
        let mut skipped = 0usize;
        let mut missing_keys = 0usize;
        for entry in entries {
            let user = entry.user.unwrap_or_else(|| fallback_user.clone());
            let duplicate = self.hosts.iter().any(|host| {
                (host.user.eq_ignore_ascii_case(&user)
                    && host.host.eq_ignore_ascii_case(&entry.host_name)
                    && host.port == entry.port)
                    || (!host.name.is_empty() && host.name.eq_ignore_ascii_case(&entry.alias))
            });
            if duplicate {
                skipped += 1;
                continue;
            }
            let (auth, key_path) = match entry.identity_file {
                Some(path) if path.is_file() => {
                    (AuthMethod::IdentityFile, path.display().to_string())
                }
                Some(_) => {
                    missing_keys += 1;
                    (AuthMethod::Agent, String::new())
                }
                None => (AuthMethod::Agent, String::new()),
            };
            let mut host = HostConfig {
                protocol: ProtocolKind::Ssh,
                id: 0,
                name: entry.alias,
                host: entry.host_name,
                port: entry.port,
                user,
                auth,
                key_path,
                key_inline: false,
                favorite: false,
                proxy_jump: None,
            };
            if self.vault.is_some() {
                host.id = self.hosts.iter().map(|item| item.id).max().unwrap_or(0) + 1;
            }
            self.hosts.push(host.clone());
            self.persist_host(&host, cx);
            imported += 1;
        }
        let mut status = format!("已从 {} 导入 {imported} 台主机", path.display());
        if skipped > 0 {
            status.push_str(&format!("，跳过 {skipped} 条重复"));
        }
        if missing_keys > 0 {
            status.push_str(&format!("，{missing_keys} 条私钥不存在改用 Agent"));
        }
        self.status = SharedString::from(status);
        cx.notify();
    }

    /// 导出主机配置 JSON。保存密码与内嵌私钥只留在凭据库中，不写入明文备份。
    pub(super) fn export_host_configs(&mut self, cx: &mut Context<Self>) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("RuTerm 主机配置", &["json"])
            .set_file_name("ruterm-hosts.json")
            .save_file()
        else {
            return;
        };
        let mut hosts = self.hosts.clone();
        let mut inline_keys = 0usize;
        for host in &mut hosts {
            if host.key_inline {
                host.key_inline = false;
                host.auth = AuthMethod::Agent;
                host.key_path.clear();
                inline_keys += 1;
            }
        }
        let backup = HostConfigBackup {
            version: 1,
            hosts,
            sidebar: self.sidebar.clone(),
        };
        match serde_json::to_vec_pretty(&backup)
            .map_err(anyhow::Error::from)
            .and_then(|json| fs::write(&path, json).map_err(anyhow::Error::from))
        {
            Ok(()) => {
                self.status = SharedString::from(format!(
                    "已导出 {} 台主机配置到 {}；密码与内嵌私钥未导出",
                    backup.hosts.len(),
                    path.display()
                ));
            }
            Err(error) => {
                self.status = SharedString::from(format!("导出主机配置失败：{error}"));
            }
        }
        if inline_keys > 0 {
            log::info!("主机配置导出省略了 {inline_keys} 个内嵌私钥");
        }
        cx.notify();
    }

    /// 从 RuTerm 主机 JSON 导入，按源主机 id 重映射跳板机引用。
    pub(super) fn import_host_configs(&mut self, cx: &mut Context<Self>) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("RuTerm 主机配置", &["json"])
            .pick_file()
        else {
            return;
        };
        let parsed = fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| {
                serde_json::from_slice::<HostConfigBackup>(&bytes).map_err(anyhow::Error::from)
            });
        let backup = match parsed {
            Ok(backup) if backup.version == 1 => backup,
            Ok(_) => {
                self.status = "主机配置文件版本不受支持".into();
                cx.notify();
                return;
            }
            Err(error) => {
                self.status = SharedString::from(format!("读取主机配置失败：{error}"));
                cx.notify();
                return;
            }
        };
        let HostConfigBackup {
            hosts,
            sidebar: imported_sidebar,
            ..
        } = backup;
        let mut valid = Vec::new();
        let mut invalid = 0usize;
        let mut seen_ids = HashSet::new();
        for mut host in hosts {
            host.key_inline = false;
            // 源 id 重复会让跳板机引用产生歧义；拒绝后出现的重复条目。
            let id_unique = host.id <= 0 || seen_ids.insert(host.id);
            if host.validate().is_some() || !id_unique {
                invalid += 1;
                continue;
            }
            valid.push(host);
        }
        let mut next_id = self.hosts.iter().map(|host| host.id).max().unwrap_or(0) + 1;
        let mut id_map = HashMap::new();
        let mut planned = Vec::new();
        let mut imported_ids = Vec::new();
        let mut duplicates = 0usize;
        for mut host in valid {
            let source_id = host.id;
            if let Some(existing) = self.hosts.iter().find(|existing| {
                existing.protocol == host.protocol
                    && existing.user.eq_ignore_ascii_case(&host.user)
                    && existing.host.eq_ignore_ascii_case(&host.host)
                    && existing.port == host.port
            }) {
                if self.vault.is_some() && source_id > 0 {
                    id_map.insert(source_id, existing.id);
                }
                duplicates += 1;
                continue;
            }
            host.id = if self.vault.is_some() { next_id } else { 0 };
            if self.vault.is_some() {
                next_id += 1;
                if source_id > 0 {
                    id_map.insert(source_id, host.id);
                }
            }
            imported_ids.push(host.id);
            planned.push(host);
        }
        let mut imported = 0usize;
        let mut broken_jumps = 0usize;
        for mut host in planned {
            if let Some(source_jump) = host.proxy_jump {
                let host_id = host.id;
                // 降级模式 id 不稳定，跳板机引用无法跟随导入；坏引用一律改为直连。
                host.proxy_jump = id_map
                    .get(&source_jump)
                    .copied()
                    .filter(|target| self.vault.is_some() && *target != host_id);
                if host.proxy_jump.is_none() {
                    broken_jumps += 1;
                }
            }
            if self.vault.is_some() {
                self.protocols.set(host.id, host.protocol);
                self.jumps.set(host.id, host.proxy_jump);
            }
            self.hosts.push(host.clone());
            self.persist_host(&host, cx);
            imported += 1;
        }
        if self.vault.is_some() {
            self.persist_protocols(cx);
            self.persist_jumps(cx);
        }
        if !imported_ids.is_empty() {
            for group in &imported_sidebar.groups {
                if !self.sidebar.names().any(|name| name == group.name) {
                    self.sidebar.groups.push(group.clone());
                }
            }
            for (source_id, group) in &imported_sidebar.assign {
                if let Some(target_id) = id_map.get(source_id)
                    && imported_ids.contains(target_id)
                    && self.sidebar.names().any(|name| name == group)
                {
                    self.sidebar.assign_to(*target_id, Some(group));
                }
            }
            for source_id in &imported_sidebar.order {
                if let Some(target_id) = id_map.get(source_id)
                    && imported_ids.contains(target_id)
                    && !self.sidebar.order.contains(target_id)
                {
                    self.sidebar.order.push(*target_id);
                }
            }
            self.persist_sidebar(cx);
        }
        self.status = SharedString::from(format!(
            "主机配置导入完成：新增 {imported} 台，重复 {duplicates} 台，跳过无效 {invalid} 台，{broken_jumps} 条跳板机引用改为直连（密码与内嵌私钥不包含在 JSON 中）"
        ));
        cx.notify();
    }

    /// 扫描局域网（「会话 → 局域网发现…」弹窗打开时触发，弹窗里可重扫）。
    pub(super) fn start_discovery(&mut self, cx: &mut Context<Self>) {
        if self.discovering {
            return;
        }
        self.discovering = true;
        self.discovery_error = None;
        self.status = tr!(cx, "正在扫描局域网…");
        cx.notify();
        let handle = rgpui::tokio::Tokio::handle(cx);
        cx.spawn(async move |this, cx| {
            let result = handle
                .spawn(async move { crate::discovery::scan_lan().await })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.discovering = false;
                this.discovery_scanned = true;
                match result {
                    Ok(Ok(hosts)) => {
                        let count = hosts.len();
                        this.discovered = hosts;
                        this.status = tr!(cx, "局域网发现 {count} 台主机", count = count);
                    }
                    Ok(Err(error)) => {
                        this.discovered.clear();
                        let message = format!("{error:#}");
                        this.discovery_error = Some(SharedString::from(message.clone()));
                        this.status = SharedString::from(format!("局域网扫描跳过：{message}"));
                    }
                    Err(error) => {
                        this.discovered.clear();
                        let message = format!("扫描任务失败：{error}");
                        this.discovery_error = Some(SharedString::from(message.clone()));
                        this.status = SharedString::from(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 打开「局域网发现」弹窗（入口在「会话」菜单）；每次打开都重扫一次。
    pub(super) fn open_discovery(&mut self, cx: &mut Context<Self>) {
        self.show_discovery = true;
        self.start_discovery(cx);
    }

    /// 关闭弹窗；扫描继续进行，结果留在内存里供下次打开查看。
    pub(super) fn close_discovery(&mut self, cx: &mut Context<Self>) {
        self.show_discovery = false;
        cx.notify();
    }

    /// 发现行按钮：按指定协议打开；打开即关闭弹窗，把位置让给表单 / 标签。
    pub(super) fn open_discovered_as(
        &mut self,
        index: usize,
        protocol: ProtocolKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.discovered.get(index) else {
            return;
        };
        let ip = host.ip.to_string();
        let port = protocol.default_port();
        self.show_discovery = false;
        self.open_form_prefilled(protocol, ip, port, window, cx);
    }

    pub(super) fn open_discovered_desktop(
        &mut self,
        index: usize,
        protocol: Protocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.discovered.get(index) else {
            return;
        };
        let port = protocol.default_port();
        self.show_discovery = false;
        self.open_desktop_prefilled(protocol, host.ip.to_string(), port, window, cx);
    }

    /// 「局域网发现」小工具弹窗：扫描状态 + 主机列表 + 每行按服务的打开按钮。
    ///
    /// 发现结果只是"临时线索"，所以不塞进主机列表、也不入库；
    /// 需要长期保留时走 SSH / Telnet 表单保存成正式主机。
    pub(super) fn render_discovery_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let count = self.discovered.len();
        let status: SharedString = if self.discovering {
            tr!(cx, "扫描中…")
        } else if let Some(error) = self.discovery_error.as_ref() {
            error.clone()
        } else if count == 0 && self.discovery_scanned {
            tr!(cx, "未发现开放 22 / 23 / 3389 / 5900 的主机")
        } else {
            tr!(cx, "共 {count} 台主机，点服务按钮直接打开", count = count)
        };
        let mut rows = v_flex()
            .id("discovery-list")
            .gap_1()
            .max_h(px(340.0))
            .overflow_y_scrollbar();
        for (index, host) in self.discovered.iter().enumerate() {
            let ip = host.ip.to_string();
            let services = host.services();
            let mut buttons = h_flex().flex_shrink_0().gap_1();
            if host.rdp {
                buttons = buttons.child(
                    Button::new(SharedString::from(format!("disc-{ip}-rdp")))
                        .label("RDP")
                        .secondary()
                        .outline()
                        .compact()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_discovered_desktop(index, Protocol::Rdp, window, cx);
                        })),
                );
            }
            if host.vnc {
                buttons = buttons.child(
                    Button::new(SharedString::from(format!("disc-{ip}-vnc")))
                        .label("VNC")
                        .secondary()
                        .outline()
                        .compact()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_discovered_desktop(index, Protocol::Vnc, window, cx);
                        })),
                );
            }
            if host.ssh {
                buttons = buttons.child(
                    Button::new(SharedString::from(format!("disc-{ip}-ssh")))
                        .label("SSH")
                        .secondary()
                        .outline()
                        .compact()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_discovered_as(index, ProtocolKind::Ssh, window, cx);
                        })),
                );
            }
            if host.telnet {
                buttons = buttons.child(
                    Button::new(SharedString::from(format!("disc-{ip}-telnet")))
                        .label("Telnet")
                        .secondary()
                        .outline()
                        .compact()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_discovered_as(index, ProtocolKind::Telnet, window, cx);
                        })),
                );
            }
            rows = rows.child(
                h_flex()
                    .id(SharedString::from(format!("disc-row-{ip}")))
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .rounded(px(5.0))
                    .hover(|style| style.bg(pal.card_hover))
                    .child(Icon::new(IconName::Frame).small().text_color(pal.muted))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().text_ellipsis().child(ip))
                            .child(
                                div()
                                    .text_xs()
                                    .text_ellipsis()
                                    .text_color(pal.muted)
                                    .child(services),
                            ),
                    )
                    .child(buttons),
            );
        }
        v_flex()
            .id("discovery-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .items_center()
            .justify_center()
            .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_discovery(cx)),
            )
            .child(
                v_flex()
                    .id("discovery-dialog")
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
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        Icon::new(IconName::Network).small().text_color(pal.link),
                                    )
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(rgpui::FontWeight(600.0))
                                            .child(tr!(cx, "局域网发现")),
                                    ),
                            )
                            .child(
                                Button::new("close-discovery").label("×").ghost().on_click(
                                    cx.listener(|this, _, _, cx| this.close_discovery(cx)),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(tr!(cx, "扫描本机各网段，探测 SSH / Telnet / RDP / VNC")),
                    )
                    .child(div().text_sm().text_color(pal.muted).child(status))
                    .when(!self.discovered.is_empty(), |this| this.child(rows))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("discovery-rescan")
                                    .label(if self.discovering {
                                        tr!(cx, "扫描中…")
                                    } else {
                                        tr!(cx, "重新扫描")
                                    })
                                    .secondary()
                                    .outline()
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.start_discovery(cx);
                                    })),
                            )
                            .child(
                                Button::new("discovery-close")
                                    .label(tr!(cx, "关闭"))
                                    .primary()
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.close_discovery(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 右键菜单"删除"：进入二次确认，不直接动手。
    pub(super) fn ask_delete(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.hosts.iter().any(|host| host.id == id) {
            self.confirm_delete = Some(id);
            cx.notify();
        }
    }

    /// 确认删除：走真实删除并持久化（连带清除库内该主机的密码/口令）。
    pub(super) fn confirm_delete_now(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.confirm_delete.take() else {
            return;
        };
        self.delete_host(id, cx);
    }

    /// 取消删除确认。
    pub(super) fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        cx.notify();
    }

    /// 删除指定主机并持久化。凭 id 定位：列表可能在确认期间被后台整表替换。
    pub(super) fn delete_host(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(index) = self.hosts.iter().position(|host| host.id == id) else {
            return;
        };
        let removed = self.hosts.remove(index);
        let label = removed.name.clone();
        self.inline_keys.remove(&removed.id);
        // 最近连接里也清掉死 id：所有使用点虽有过滤自愈，但脏数据会随
        // persist_recent_hosts 一路写回库，不如删的时候顺手清干净。
        self.recent_hosts.retain(|recent| *recent != removed.id);
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
        self.sidebar.forget(removed.id);
        self.persist_sidebar(cx);
        // 跳板机引用一并断掉：删的是跳板机时，原本经由它的主机改回直连。
        if self.vault.is_some() {
            let orphaned = self.jumps.forget(removed.id);
            for host in &mut self.hosts {
                if orphaned.contains(&host.id) {
                    host.proxy_jump = None;
                }
            }
            self.persist_jumps(cx);
            self.protocols.forget(removed.id);
            self.persist_protocols(cx);
        }
        self.status = SharedString::from(format!("已删除主机 {label}"));
        cx.notify();
    }

    /// 折叠 / 展开分组（文件夹）。
    pub(super) fn toggle_group(&mut self, name: &str, cx: &mut Context<Self>) {
        self.sidebar.toggle(name);
        cx.notify();
        self.persist_sidebar(cx);
    }

    /// 打开分组新建 / 重命名弹窗；`Some(原名)` 表示重命名。
    pub(super) fn open_group_prompt(
        &mut self,
        renaming: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = renaming.clone().unwrap_or_else(|| "".into());
        let placeholder = if renaming.is_some() {
            "分组名称"
        } else {
            "如：生产环境 / 家"
        };
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        if !initial.is_empty() {
            name.update(cx, |state, cx| state.set_value(initial, window, cx));
        }
        // 回车即确认，与解锁弹窗一致。
        cx.subscribe(&name, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                this.commit_group_prompt(cx);
            }
        })
        .detach();
        name.update(cx, |state, cx| state.focus(window, cx));
        self.group_prompt = Some(GroupPrompt { renaming, name });
        cx.notify();
    }

    /// 关闭分组弹窗，不写入。
    pub(super) fn cancel_group_prompt(&mut self, cx: &mut Context<Self>) {
        self.group_prompt = None;
        cx.notify();
    }

    /// 确认分组弹窗：新建或重命名。空名 / 重名只提示，不关窗，便于就地改。
    pub(super) fn commit_group_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.group_prompt.clone() else {
            return;
        };
        let name = prompt.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.status = "分组名称不能为空".into();
            cx.notify();
            return;
        }
        let ok = match prompt.renaming.as_deref() {
            Some(from) => self.sidebar.rename(from, &name),
            None => self.sidebar.add(&name),
        };
        if !ok {
            self.status = if prompt.renaming.is_some() {
                "重命名失败：名称为空或已存在同名分组".into()
            } else {
                "已存在同名分组".into()
            };
            cx.notify();
            return;
        }
        if let Some(from) = prompt.renaming.as_deref() {
            self.status = SharedString::from(format!("分组 {from} 已改名为 {name}"));
        } else {
            self.status = SharedString::from(format!("已新建分组 {name}"));
        }
        self.group_prompt = None;
        cx.notify();
        self.persist_sidebar(cx);
    }

    /// 删除分组：只退回归属，主机本身保留。
    pub(super) fn delete_group(&mut self, name: &str, cx: &mut Context<Self>) {
        self.sidebar.remove(name);
        self.status = SharedString::from(format!("已删除分组 {name}（组内主机退回未分组）"));
        cx.notify();
        self.persist_sidebar(cx);
    }

    /// 把主机移入分组（`None` = 未分组）。
    pub(super) fn move_host(
        &mut self,
        id: i64,
        group: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if !self.hosts.iter().any(|host| host.id == id) {
            return;
        }
        self.sidebar.assign_to(id, group.as_deref());
        cx.notify();
        self.persist_sidebar(cx);
    }

    /// 组内手动排序：收藏 / 非收藏各自成桶（收藏恒在桶上方，菜单只挪同桶邻居）。
    pub(super) fn reorder_host(&mut self, id: i64, action: ReorderAction, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter().find(|host| host.id == id) else {
            return;
        };
        let (id, favorite) = (host.id, host.favorite);
        let group = self
            .sidebar
            .group_of(id)
            .map(str::to_string)
            .unwrap_or_else(|| UNGROUPED.to_string());
        let alive: Vec<i64> = self.hosts.iter().map(|host| host.id).collect();
        self.sidebar.sync_order(&alive);
        // 同组同桶的主机按当前展示顺序（手动序 → 插入序）排好。
        let mut peers: Vec<i64> = self
            .hosts
            .iter()
            .filter(|candidate| candidate.favorite == favorite)
            .filter(|candidate| {
                self.sidebar
                    .group_of(candidate.id)
                    .map(str::to_string)
                    .unwrap_or_else(|| UNGROUPED.to_string())
                    == group
            })
            .map(|candidate| candidate.id)
            .collect();
        peers.sort_by_key(|&peer| self.sidebar.rank_of(peer));
        let Some(position) = peers.iter().position(|&peer| peer == id) else {
            return;
        };
        let anchor = match action {
            ReorderAction::Up if position > 0 => Some(peers[position - 1]),
            ReorderAction::Down if position + 1 < peers.len() => Some(peers[position + 1]),
            ReorderAction::Top if position > 0 => Some(peers[0]),
            _ => None,
        };
        let Some(anchor) = anchor else {
            let boundary = match action {
                ReorderAction::Down => "已在组内最后",
                _ => "已在组内最前",
            };
            self.status = SharedString::from(boundary);
            cx.notify();
            return;
        };
        match action {
            // 下移＝把下一个邻居挪到本机前面，等价于本机后移一位。
            ReorderAction::Down => self.sidebar.move_before(anchor, id),
            _ => self.sidebar.move_before(id, anchor),
        }
        self.persist_sidebar(cx);
        cx.notify();
    }

    /// 切换侧边栏排序方式并持久化。
    pub(super) fn set_host_sort(&mut self, sort: SidebarSort, cx: &mut Context<Self>) {
        if self.sidebar.set_sort(sort) {
            self.persist_sidebar(cx);
        }
        cx.notify();
    }

    /// 切换指定主机的收藏状态并持久化。
    pub(super) fn toggle_favorite(&mut self, id: i64, cx: &mut Context<Self>) {
        if let Some(host) = self.hosts.iter_mut().find(|host| host.id == id) {
            host.favorite = !host.favorite;
            let host = host.clone();
            self.persist_host(&host, cx);
            cx.notify();
        }
    }

    /// 主机行图标：Telnet 用地球标；SSH 按认证方式区分，一眼看出连这台机器要什么凭据。
    pub(super) fn host_icon(host: &HostConfig) -> IconName {
        match host.protocol {
            ProtocolKind::Telnet => IconName::Globe,
            // 桌面协议共用「窗口」图标：当前图标集里没有显示器图标。
            ProtocolKind::Rdp | ProtocolKind::Vnc => IconName::Frame,
            ProtocolKind::Ssh => match host.auth {
                AuthMethod::Agent => IconName::Network,
                AuthMethod::IdentityFile => IconName::File,
                AuthMethod::Password => IconName::CircleUser,
            },
        }
    }

    /// 列表副标题：Telnet / 远程桌面前缀协议名，避免与 SSH 主机混淆。
    pub(super) fn host_subtitle(host: &HostConfig) -> String {
        match host.protocol {
            ProtocolKind::Ssh => host.addr(),
            ProtocolKind::Telnet => format!("Telnet · {}", host.addr()),
            ProtocolKind::Rdp => format!("RDP · {}", host.addr()),
            ProtocolKind::Vnc => format!("VNC · {}", host.addr()),
        }
    }

    /// 列表里显示的主机名：没填展示名时退回 user@host:port。
    pub(super) fn host_title(host: &HostConfig) -> String {
        if host.name.is_empty() {
            host.addr()
        } else {
            host.name.clone()
        }
    }

    /// 列表行的通用外观：透明底 + 悬停高亮（树式，不再是每张卡片一块底），
    /// 左侧留出的缩进由 `indent` 控制，分组下的主机比文件夹行多缩一档。
    pub(super) fn row_base(
        id: impl Into<SharedString>,
        pal: &Palette,
        indent: Pixels,
    ) -> Stateful<Div> {
        h_flex()
            .id(id.into())
            .w_full()
            .pl(indent)
            .pr_2()
            .py_1()
            .gap_2()
            .items_center()
            .rounded(px(5.0))
            .hover(|style| style.bg(pal.card_hover))
            .cursor_pointer()
    }

    /// 分组（文件夹）行：折叠三角 + 文件夹图标 + 组名 + 数量；
    /// 点整行折叠/展开，右键给出该组的分组管理与新建入口。
    pub(super) fn render_group_row(
        &self,
        name: &str,
        count: usize,
        collapsed: bool,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let groups: Vec<SharedString> = self
            .sidebar
            .names()
            .map(|n| SharedString::from(n.to_string()))
            .collect();
        let toggle_name = SharedString::from(name.to_string());
        let menu_name = toggle_name.clone();
        let sort = self.sidebar.sort;
        Self::row_base(
            SharedString::from(format!("group-row-{name}")),
            pal,
            px(2.0),
        )
        .font_weight(rgpui::FontWeight(600.0))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_group(&toggle_name, cx);
        }))
        .child(
            Icon::new(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .xsmall()
            .text_color(pal.muted),
        )
        .child(
            Icon::new(if collapsed {
                IconName::FolderClosed
            } else {
                IconName::FolderOpen
            })
            .small()
            .text_color(pal.link),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_ellipsis()
                .child(name.to_string()),
        )
        .when(count > 0, |this| {
            this.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(count.to_string()),
            )
        })
        .context_menu(move |menu, _, _| {
            Self::context_menu(
                MenuTarget::Group(menu_name.clone()),
                groups.clone(),
                None,
                sort,
                entity.clone(),
                menu,
            )
        })
        .into_any_element()
    }

    /// 一台主机：认证方式图标 + 展示名 + user@host:port + ★。
    pub(super) fn render_host_row(
        &self,
        index: usize,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let host = &self.hosts[index];
        // 闭包的生命周期可能横跨后台 attach_vault 的整表替换，一律凭 id
        // 定位，动作时再解析成当前下标。
        let id = host.id;
        let entity = cx.entity();
        let groups: Vec<SharedString> = self
            .sidebar
            .names()
            .map(|n| SharedString::from(n.to_string()))
            .collect();
        let current = self
            .sidebar
            .group_of(host.id)
            .map(|name| SharedString::from(name.to_string()));
        let fav_glyph: &'static str = if host.favorite { "★" } else { "☆" };
        // 没填展示名时标题直接用地址，避免整行只剩一行灰字。
        let title = Self::host_title(host);
        let sort = self.sidebar.sort;
        Self::row_base(
            SharedString::from(format!("host-row-{index}")),
            pal,
            px(22.0),
        )
        // 双击才连接：手快点两下不会误开两条会话。
        .on_double_click(cx.listener(move |this, _, window, cx| {
            this.click_host(id, window, cx);
        }))
        .child(
            Icon::new(Self::host_icon(host))
                .small()
                .text_color(if host.favorite { pal.accent } else { pal.muted }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(
                    div()
                        .text_ellipsis()
                        .font_weight(rgpui::FontWeight(500.0))
                        .child(title),
                )
                .child(
                    div()
                        .text_ellipsis()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(Self::host_subtitle(host)),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("host-fav-{index}")))
                .flex_shrink_0()
                .text_sm()
                .px_1()
                .text_color(if host.favorite { pal.accent } else { pal.muted })
                .child(fav_glyph)
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_favorite(id, cx))),
        )
        .context_menu(move |menu, _, _| {
            Self::context_menu(
                MenuTarget::HostId(id),
                groups.clone(),
                current.clone(),
                sort,
                entity.clone(),
                menu,
            )
        })
        .into_any_element()
    }

    /// 主机分组树：内置「本地终端」→ 各自定义分组 → 未分组 → 空白区。
    /// 分组为空也照样显示（和参考客户端一致：先建组，再往里挪主机）。
    pub(super) fn render_host_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        // 按展示名 / user@host:port 子串过滤；保留真实下标供点击/删除定位。
        let matched: Vec<usize> = self
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
        // 收藏主机置顶；同桶内按侧边栏当前选择的排序方式（手动 / 名称 / 最近连接）。
        let mut rows: Vec<(usize, String)> = matched
            .into_iter()
            .map(|index| (index, Self::host_title(&self.hosts[index])))
            .collect();
        rows.sort_by(|(a_index, a_title), (b_index, b_title)| {
            let a = &self.hosts[*a_index];
            let b = &self.hosts[*b_index];
            self.sidebar.compare_hosts(
                SortableHost {
                    id: a.id,
                    title: a_title,
                    favorite: a.favorite,
                },
                SortableHost {
                    id: b.id,
                    title: b_title,
                    favorite: b.favorite,
                },
                &self.recent_hosts,
            )
        });
        // 分桶：先按偏好里的组顺序建桶，末尾恒有「未分组」。
        let mut buckets: Vec<(String, Vec<usize>)> = self
            .sidebar
            .names()
            .map(|name| (name.to_string(), Vec::new()))
            .chain(std::iter::once((UNGROUPED.to_string(), Vec::new())))
            .collect();
        for (index, _) in rows {
            let group = self
                .sidebar
                .group_of(self.hosts[index].id)
                .unwrap_or(UNGROUPED)
                .to_string();
            match buckets.iter_mut().find(|(name, _)| *name == group) {
                Some((_, rows)) => rows.push(index),
                // 归属指向一个已被删掉的组：退回未分组（group_of 已过滤，这里兜底）。
                None => {
                    if let Some((_, rows)) = buckets.last_mut() {
                        rows.push(index);
                    }
                }
            }
        }
        // 内置「本地终端」条目恒在最前（不入库，故也不受删除/分组/收藏影响）。
        let local_rows = self.render_local_terminal_rows(&pal, &query, cx);
        // 列表末尾的空白区：右键给出「仅新建」的菜单。它是行的兄弟节点而非祖先，
        // 否则一次右键会同时命中两层菜单，弹出两个重叠的浮层。
        let blank = {
            let entity = cx.entity();
            let groups: Vec<SharedString> = self
                .sidebar
                .names()
                .map(|n| SharedString::from(n.to_string()))
                .collect();
            let sort = self.sidebar.sort;
            div()
                .id("sidebar-blank")
                .w_full()
                .min_h(px(80.0))
                .flex_1()
                .context_menu(move |menu, _, _| {
                    Self::context_menu(
                        MenuTarget::Blank,
                        groups.clone(),
                        None,
                        sort,
                        entity.clone(),
                        menu,
                    )
                })
        };
        let searching = !query.is_empty();
        // 搜索命中本机 shell 时，主机没有命中也不该再叠一句"无匹配主机"。
        let local_hit = !local_rows.is_empty();
        let mut list = v_flex().gap_px().min_h_full().children(local_rows);
        for (name, rows) in &buckets {
            // 「未分组」只是兜底桶：没有未分组主机时根本不渲染——否则刚建完分组、一台主机都没加时，
            // 它会和空的新组并排出现，看着像凭空多了一个组。自定义空组保留（那是有意建的容器）。
            // 搜索态下空组一律隐藏、折叠强制展开，否则命中的主机看不见。
            if rows.is_empty() && (searching || name == UNGROUPED) {
                continue;
            }
            let collapsed = !searching && self.sidebar.collapsed(name);
            list = list.child(self.render_group_row(name, rows.len(), collapsed, &pal, cx));
            if collapsed {
                continue;
            }
            list = list.children(
                rows.iter()
                    .map(|&index| self.render_host_row(index, &pal, cx)),
            );
        }
        // 树顶已有本机 shell 列表，首启动不再塞"暂无主机"那行大字（空着比提示好看）；
        // 只在搜索没命中时给一行反馈。
        let host_hit = buckets.iter().any(|(_, rows)| !rows.is_empty());
        if searching && !local_hit && !host_hit {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(pal.muted)
                    .child("无匹配主机"),
            );
        }
        list.child(blank).into_any_element()
    }

    /// 分组新建 / 重命名弹窗：与主机表单同款居中模态。
    /// 遮罩只挡交互不关闭（防止误丢已输入的名称），关闭走"取消"或标题栏 ×。
    pub(super) fn render_group_prompt(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(prompt) = self.group_prompt.as_ref() else {
            return div().into_any_element();
        };
        let renaming = prompt.renaming.clone();
        let title = match &renaming {
            Some(old) => format!("重命名分组：{old}"),
            None => "新建分组".to_string(),
        };
        v_flex()
            .id("group-backdrop")
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
                    .id("group-form")
                    .w(px(360.0))
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
                                    .child(title),
                            )
                            .child(Button::new("close-group").label("×").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.cancel_group_prompt(cx);
                                }),
                            )),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child("分组名称"))
                            .child(styled_input(Input::new(&prompt.name), &pal)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child("分组只影响列表的组织方式，不改动主机本身。"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("commit-group")
                                    .label(if renaming.is_some() {
                                        "重命名"
                                    } else {
                                        "创建"
                                    })
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.commit_group_prompt(cx);
                                    })),
                            )
                            .child(Button::new("cancel-group").label("取消").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.cancel_group_prompt(cx);
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }

    /// 空工作区首页：品牌 + 主机搜索 + 新建入口 + 最近连接。
    ///
    /// 参照路线图「界面与交互」① 的空状态，但配色、图标与控件
    /// 全部走主题与 `IconName`：搜索里敲字即列匹配主机，清空则回到最近连接。
    pub(super) fn render_empty_state(&self, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let query = self.home_search.read(cx).value().trim().to_lowercase();
        // 首页不做长列表：搜索最多 8 条，最近连接最多 6 条。
        let (title, rows): (SharedString, Vec<usize>) = if query.is_empty() {
            (
                tr!(cx, "最近连接"),
                self.recent_hosts
                    .iter()
                    .filter_map(|id| self.hosts.iter().position(|host| host.id == *id))
                    .take(6)
                    .collect(),
            )
        } else {
            (
                tr!(cx, "匹配的主机"),
                self.hosts
                    .iter()
                    .enumerate()
                    .filter(|(_, host)| {
                        host.name.to_lowercase().contains(&query)
                            || host.addr().to_lowercase().contains(&query)
                    })
                    .map(|(index, _)| index)
                    .take(8)
                    .collect(),
            )
        };
        let empty_hint = if query.is_empty() {
            tr!(cx, "还没有连接记录，从左侧主机列表选一台即可开始")
        } else {
            tr!(cx, "没有匹配的主机")
        };
        let subtitle = tr!(cx, "远程终端会话管理器 · SSH / Telnet / 本地终端");
        let new_host_label = tr!(cx, "新建主机");
        let local_term_label = tr!(cx, "本地终端");
        let mut card = v_flex()
            .w_full()
            .rounded(px(8.0))
            .border_1()
            .border_color(pal.border)
            .bg(pal.card_bg)
            .p_2()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(title),
            );
        if rows.is_empty() {
            card = card.child(
                div()
                    .w_full()
                    .py_3()
                    .text_xs()
                    .text_color(pal.muted)
                    .text_center()
                    .child(empty_hint),
            );
        } else {
            for index in rows {
                card = card.child(self.render_home_host_row(index, pal, cx));
            }
        }
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(
                v_flex()
                    .w(px(560.0))
                    .gap_4()
                    .items_center()
                    .child(
                        v_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(30.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(pal.text)
                                    .child("RuTerm"),
                            )
                            .child(div().text_sm().text_color(pal.muted).child(subtitle)),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(styled_input(Input::new(&self.home_search), pal)),
                            )
                            .child(
                                Button::new("home-new-host")
                                    .icon(IconName::Plus)
                                    .label(new_host_label)
                                    .primary()
                                    .compact()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_host(&NewHost, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("home-local-terminal")
                                    .icon(IconName::SquareTerminal)
                                    .label(local_term_label)
                                    .secondary()
                                    .outline()
                                    .compact()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_local_terminal(&NewLocalTerminal, window, cx);
                                    })),
                            ),
                    )
                    .child(card),
            )
            .into_any_element()
    }

    /// 首页里的一台主机：图标 + 展示名 + 地址，双击连接（与侧边栏一致，避免误触连开两次）。
    pub(super) fn render_home_host_row(
        &self,
        index: usize,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let host = &self.hosts[index];
        let id = host.id;
        let title = if host.name.is_empty() {
            host.addr()
        } else {
            host.name.clone()
        };
        Self::row_base(
            SharedString::from(format!("home-host-{index}")),
            pal,
            px(8.0),
        )
        .on_double_click(cx.listener(move |this, _, window, cx| {
            this.click_host(id, window, cx);
        }))
        .child(
            Icon::new(Self::host_icon(host))
                .small()
                .text_color(pal.muted),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .text_ellipsis()
                        .font_weight(rgpui::FontWeight(500.0))
                        .child(title),
                )
                .child(
                    div()
                        .text_xs()
                        .text_ellipsis()
                        .text_color(pal.muted)
                        .child(Self::host_subtitle(host)),
                ),
        )
        .into_any_element()
    }

    /// 连接成功（shell 已开）后记一笔最近连接：去重后置顶，超出上限截断。
    /// 降级模式（无加密库）主机 id 不可靠，只保留内存值，不落盘。
    pub(super) fn record_recent_host(&mut self, host_id: i64, cx: &mut Context<Self>) {
        if host_id == 0 || !self.hosts.iter().any(|host| host.id == host_id) {
            return;
        }
        self.recent_hosts.retain(|id| *id != host_id);
        self.recent_hosts.insert(0, host_id);
        self.recent_hosts.truncate(RECENT_HOSTS_MAX);
        self.persist_recent_hosts(cx);
    }

    pub(super) fn persist_recent_hosts(&self, cx: &mut Context<Self>) {
        let Some(vault) = self.vault.clone() else {
            return;
        };
        let ids = self.recent_hosts.clone();
        cx.spawn(async move |_, _| {
            let Ok(json) = serde_json::to_string(&ids) else {
                return;
            };
            if let Err(error) = vault.set_setting("recent", &json).await {
                log::warn!("写入库内最近连接失败: {error}");
            }
        })
        .detach();
    }
}
