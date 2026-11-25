//! //! 新建 / 编辑主机表单：字段收集、私钥（文件 / 内嵌）保存、跳板机与协议偏好持久化。

use rgpui::components::Select;
use rgpui::input_ui::{Input, InputContentType, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, Entity, FontWeight, InteractiveElement,
    IntoElement, MouseButton, ParentElement, ScrollableElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, h_flex, px, v_flex,
};

use crate::NewHost;
use crate::sessions::{AuthMethod, HostConfig, ProtocolKind};
use crate::tr;

use super::remote_desktop::{desktop_cache_key, store_desktop_secret_after_save};
use super::*;

/// 新建/编辑主机表单（各字段为受控输入状态）。
pub(super) struct NewHostForm {
    /// 连接协议：决定下方哪些字段与认证方式有效。
    pub(super) protocol: ProtocolKind,
    pub(super) name: Entity<InputState>,
    pub(super) host: Entity<InputState>,
    pub(super) port: Entity<InputState>,
    pub(super) user: Entity<InputState>,
    pub(super) key_path: Entity<InputState>,
    /// 私钥来源：true＝内嵌到加密库（PEM 存库），false＝本地文件路径。
    pub(super) key_inline: bool,
    /// 本次表单里新导入的私钥 PEM（仅内存；保存时才写入加密库）。
    pub(super) key_data: Option<String>,
    /// 导入的私钥文件名，仅用于界面提示（绝不展示内容）。
    pub(super) key_label: Option<String>,
    /// 加密私钥口令（可选）。保存时勾选入库则写加密库，否则仅本次内存缓存；绝不落明文。
    pub(super) passphrase: Entity<InputState>,
    /// 远程桌面（RDP / VNC）新密码。已保存的密码不会回填显示；留空＝不改动。
    pub(super) password: Entity<InputState>,
    /// 表单层"保存口令到加密库"开关（默认否＝仅本次内存持有）。
    pub(super) save_secret: bool,
    pub(super) auth: AuthMethod,
    /// 保存到的分组；None＝未分组。从分组右键进入时预填该组，编辑时预填原归属。
    pub(super) group: Option<SharedString>,
    /// 跳板机（另一台已保存主机的 id）；None＝直连。
    pub(super) jump: Option<i64>,
    /// Some(id)＝编辑既有主机（保存时按该 id 原位替换）。
    /// 用 id 而非下标：表单开着时列表可能被后台整表替换，下标会指向别的行。
    pub(super) editing: Option<i64>,
}

impl RuTermApp {
    /// 打开新建主机表单（未分组）。
    pub(super) fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form_at(None, None, window, cx);
    }

    /// 打开新建主机表单并预选分组（分组右键「新建主机…」进入）。
    pub(super) fn open_form_in_group(
        &mut self,
        group: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_form_at(None, group, window, cx);
    }

    /// 打开新建 / 编辑表单；edit_id 为 Some 时以该主机字段预填。
    /// `group` 是新建主机的预选分组（编辑时为 None，按原归属预填）。
    pub(super) fn open_form_at(
        &mut self,
        edit_id: Option<i64>,
        group: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_form_with(edit_id, None, group, window, cx);
    }

    /// 从局域网发现新建主机：协议 / 地址 / 端口预填，认证方式留给表单里的用户。
    pub(super) fn open_form_prefilled(
        &mut self,
        protocol: ProtocolKind,
        host: String,
        port: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_form_with(None, Some((protocol, host, port)), None, window, cx);
    }

    fn open_form_with(
        &mut self,
        edit_id: Option<i64>,
        seed: Option<(ProtocolKind, String, u16)>,
        group: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = edit_id.and_then(|id| self.hosts.iter().find(|host| host.id == id).cloned());
        let editing = existing.is_some();
        let protocol = existing
            .as_ref()
            .map(|host| host.protocol)
            .or_else(|| seed.as_ref().map(|(protocol, _, _)| *protocol))
            .unwrap_or_default();
        let auth = existing
            .as_ref()
            .map(|host| host.auth)
            .unwrap_or(AuthMethod::Agent);
        let edit_id = if editing { edit_id } else { None };
        // 新建用调用方给的分组；编辑回填主机当前归属（可在表单里改）。
        let group = match existing.as_ref() {
            Some(host) => self
                .sidebar
                .group_of(host.id)
                .map(|name| SharedString::from(name.to_string())),
            None => group,
        };
        let placeholder_name: SharedString = if editing {
            "".into()
        } else {
            tr!(cx, "展示名（留空用主机地址）")
        };
        let form = NewHostForm {
            protocol,
            name: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder(placeholder_name);
                match &existing {
                    Some(host) => input.default_value(host.name.clone()),
                    None => input,
                }
            }),
            host: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder(tr!(cx, "host 或 IP"));
                match &existing {
                    Some(host) => input.default_value(host.host.clone()),
                    None => match &seed {
                        Some((_, host, _)) => input.default_value(host.clone()),
                        None => input,
                    },
                }
            }),
            port: cx.new(|cx| {
                let default_port = seed
                    .as_ref()
                    .map(|(_, _, port)| port.to_string())
                    .unwrap_or_else(|| protocol.default_port().to_string());
                match &existing {
                    Some(host) => InputState::new(window, cx).default_value(host.port.to_string()),
                    None => InputState::new(window, cx).default_value(default_port),
                }
            }),
            user: cx.new(|cx| {
                let input = InputState::new(window, cx).placeholder(tr!(cx, "登录用户名"));
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
                InputState::new(window, cx)
                    .placeholder(tr!(cx, "私钥口令（可选，留空则连接时按需询问）"))
            }),
            password: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!(cx, "已保存密码不会显示，留空则不改动"))
            }),
            key_inline: existing.as_ref().is_some_and(|host| host.key_inline),
            key_data: None,
            key_label: None,
            save_secret: false,
            auth,
            group,
            jump: existing.as_ref().and_then(|host| host.proxy_jump),
            editing: edit_id,
        };
        self.new_host = Some(form);
        // 表单打开即把焦点交给"主机地址"输入框：退格/删除等键位动作只在输入框
        // 持有焦点（"Input" 键位上下文激活）时才会派发，主动聚焦让删除键即时可用。
        if let Some(form) = self.new_host.as_ref() {
            form.host.update(cx, |state, cx| state.focus(window, cx));
        }
        self.status = if editing {
            tr!(cx, "编辑主机")
        } else {
            tr!(cx, "填写主机信息")
        };
        cx.notify();
    }

    /// 关闭新建表单（不保存）。
    pub(super) fn cancel_form(&mut self, cx: &mut Context<Self>) {
        self.new_host = None;
        self.status = tr!(cx, "就绪");
        cx.notify();
    }

    /// 保存新建/编辑表单到列表并持久化；校验失败则保留表单。
    pub(super) fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.new_host.as_ref() else {
            return;
        };
        let editing = form.editing;
        let protocol = form.protocol;
        // 只有 SSH 使用认证材料：Telnet / 桌面协议一律归零，
        // 避免把旧 SSH 主机的私钥配置带到别的会话里。
        let auth = if protocol == ProtocolKind::Ssh {
            form.auth
        } else {
            AuthMethod::Agent
        };
        let passphrase = form.passphrase.read(cx).value().trim().to_string();
        let password = form.password.read(cx).value().trim().to_string();
        let save_secret = form.save_secret;
        let group = form.group.clone();
        // 跳板机是 SSH 专属：切到其它协议时连同旧值一起清掉，
        // 避免之后被当成 SSH 跳板机去连接一个非 SSH 端口。
        let jump = if protocol == ProtocolKind::Ssh {
            form.jump
        } else {
            None
        };
        let key_inline = protocol == ProtocolKind::Ssh && form.key_inline;
        let key_data = form.key_data.clone();
        let host_raw = form.host.read(cx).value().trim().to_string();
        let key_path = form.key_path.read(cx).value().trim().to_string();
        // 端口留空沿用协议默认；填了但解析不出（如 22q2、超 u16 的 99999）必须
        // 显式报错——静默改写成默认端口会让「保存的」和「输入的」悄悄不一致，
        // 之后连接失败极难归因。
        let port_raw = form.port.read(cx).value().trim().to_string();
        let port = if port_raw.is_empty() {
            protocol.default_port()
        } else {
            match port_raw.parse::<u16>() {
                Ok(port) if port != 0 => port,
                _ => {
                    self.status = tr!(
                        cx,
                        "保存失败：端口「{port_raw}」无效（应为 1-65535 的数字）",
                        port_raw = port_raw
                    );
                    cx.notify();
                    return;
                }
            }
        };
        let mut host = HostConfig {
            protocol,
            id: 0,
            name: form.name.read(cx).value().trim().to_string(),
            host: host_raw.clone(),
            port,
            user: form.user.read(cx).value().trim().to_string(),
            auth,
            key_path: if key_inline { String::new() } else { key_path },
            key_inline: auth == AuthMethod::IdentityFile && key_inline,
            favorite: false,
            proxy_jump: jump,
        };
        if host.protocol == ProtocolKind::Ssh && host.key_inline {
            if self.vault.is_none() {
                self.status = tr!(
                    cx,
                    "内嵌私钥需要加密凭据库（当前不可用），请改用本地私钥文件"
                );
                cx.notify();
                return;
            }
            let existing = editing
                .and_then(|id| self.hosts.iter().find(|host| host.id == id))
                .is_some_and(|current| current.key_inline);
            if key_data.is_none() && !existing {
                self.status = tr!(
                    cx,
                    "请先导入私钥文件（选择「内嵌到加密库」后点「导入私钥…」）"
                );
                cx.notify();
                return;
            }
        }
        if let Some(error) = host.validate() {
            log::warn!("保存主机失败: {error}");
            self.status = tr!(cx, "保存失败：{error}", error = error);
            cx.notify();
            return;
        }
        // 跳板机引用必须是仍存在的另一台主机，且不能自己挂自己或形成两级以上链。
        if let Some(jump_id) = host.proxy_jump {
            let self_id = editing
                .and_then(|id| self.hosts.iter().find(|host| host.id == id))
                .map(|host| host.id);
            match self.hosts.iter().find(|item| item.id == jump_id) {
                None => {
                    self.status = tr!(cx, "所选跳板机已不存在，请重新选择");
                    cx.notify();
                    return;
                }
                Some(jump) if Some(jump.id) == self_id => {
                    self.status = tr!(cx, "不能把主机自己设为跳板机");
                    cx.notify();
                    return;
                }
                Some(jump) if jump.proxy_jump.is_some() => {
                    self.status = tr!(cx, "跳板机不能再挂跳板机，请先解除它的跳板机设置");
                    cx.notify();
                    return;
                }
                Some(_) => {}
            }
        }
        let label = if host.name.is_empty() {
            host.host.clone()
        } else {
            host.name.clone()
        };
        let group_note = group
            .as_deref()
            .map(|name| tr!(cx, "（分组：{name}）", name = name).to_string())
            .unwrap_or_default();
        if let Some(edit_id) = editing {
            // 原位替换：id 与收藏标记沿用原主机；已保存的密码/口令不受影响。
            // 凭 id 定位——列表可能在表单打开期间被后台整表替换。
            if let Some(index) = self.hosts.iter().position(|item| item.id == edit_id) {
                let current = self.hosts[index].clone();
                host.id = current.id;
                host.favorite = current.favorite;
                let was_inline = current.key_inline;
                self.hosts[index] = host.clone();
                self.persist_host(&host, cx);
                self.note_jump(&host, cx);
                self.note_protocol(&host, cx);
                let note = self.store_form_secret(&host, passphrase.clone(), save_secret, cx);
                let password_note =
                    self.store_desktop_form_secret(&host, password.clone(), save_secret, cx);
                let key_note = self.store_form_key(&host, key_data, was_inline, cx);
                self.apply_form_group(&host, group, cx);
                self.new_host = None;
                self.status = tr!(
                    cx,
                    "已更新主机 {label}{notes}{group_note}",
                    label = label,
                    notes = join_notes(Some(join_notes(note, password_note)), key_note),
                    group_note = group_note,
                );
                cx.notify();
                return;
            }
            // 编辑态找不到原主机（打开期间被并发替换 / 删除）：明确告知，
            // 别让「我以为在编辑」悄悄变成「多出一台新主机」。
            self.status = tr!(cx, "原主机已不存在，本次保存按新主机写入");
        }
        // 新建：加密库可用时同步分配主键（max+1，与库内 INSERT 显式 id 对齐）；
        // 降级模式沿用旧 JSON（id=0，boot_vault 成功回填时会补发）。
        if self.vault.is_some() {
            host.id = self.hosts.iter().map(|item| item.id).max().unwrap_or(0) + 1;
        }
        self.hosts.push(host.clone());
        self.persist_host(&host, cx);
        self.note_jump(&host, cx);
        self.note_protocol(&host, cx);
        let note = self.store_form_secret(&host, passphrase, save_secret, cx);
        let password_note = self.store_desktop_form_secret(&host, password, save_secret, cx);
        let key_note = self.store_form_key(&host, key_data, false, cx);
        self.apply_form_group(&host, group, cx);
        self.new_host = None;
        self.status = tr!(
            cx,
            "已保存主机 {label}{notes}{group_note}",
            label = label,
            notes = join_notes(Some(join_notes(note, password_note)), key_note),
            group_note = group_note,
        );
        cx.notify();
    }

    /// 把刚保存的主机落到表单选定的分组，并展开该组让新主机立刻可见。
    /// 未分组（None）同时清掉旧归属。
    pub(super) fn apply_form_group(
        &mut self,
        host: &HostConfig,
        group: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let name = group.as_ref().map(|name| name.to_string());
        self.sidebar.assign_to(host.id, name.as_deref());
        if let Some(name) = name.as_deref() {
            self.sidebar.expand(name);
        }
        self.persist_sidebar(cx);
    }

    /// 表单私钥口令的两档落点：勾选入库（加密库）或仅本次内存缓存。
    /// 空白口令＝不改动既有秘密。返回附注文案（None 表示无需提示）。
    pub(super) fn store_form_secret(
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
                return Some(tr!(cx, "（口令已加密入库）").to_string());
            }
            // 加密库不可用：绝不降级为明文保存，退回本次内存持有。
            self.password_cache.insert(host.addr(), passphrase);
            return Some(tr!(cx, "（加密库不可用，口令仅本次内存持有）").to_string());
        }
        self.password_cache.insert(host.addr(), passphrase);
        Some(tr!(cx, "（口令仅本次内存持有）").to_string())
    }

    /// 远程桌面主机密码的落点。与 SSH 口令共用「保存口令」开关，但按桌面的
    /// 缓存键与凭据库写入路径处理；空密码＝不改动已保存的凭据。
    pub(super) fn store_desktop_form_secret(
        &mut self,
        host: &HostConfig,
        password: String,
        save: bool,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if !matches!(host.protocol, ProtocolKind::Rdp | ProtocolKind::Vnc) || password.is_empty() {
            return None;
        }
        // 进程内缓存按桌面口径（账号 + 地址 + 端口）记，连接时才能直接命中。
        let key = desktop_cache_key(&host.host, host.port, &host.user, "");
        self.password_cache.insert(key, password.clone());
        if save {
            let written =
                store_desktop_secret_after_save(self.vault.clone(), host.clone(), &password, cx);
            return Some(
                if written {
                    tr!(cx, "（密码已加密入库）")
                } else {
                    tr!(cx, "（加密库不可用，密码仅本次内存持有）")
                }
                .to_string(),
            );
        }
        Some(tr!(cx, "（密码仅本次内存持有）").to_string())
    }

    /// 内嵌私钥的落点：新导入的写入加密库，取消内嵌则清除库内旧条目。
    ///
    /// 编辑时没重新导入（key_data 为空）且原本就是内嵌的，视为"保持原样"；
    /// 删除永远不会降级为明文——加密库不可用时内嵌选项根本不可选。
    pub(super) fn store_form_key(
        &mut self,
        host: &HostConfig,
        key_data: Option<String>,
        was_inline: bool,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if host.key_inline {
            let pem = key_data?;
            if let (Some(vault), true) = (self.vault.clone(), host.id != 0) {
                let id = host.id;
                self.inline_keys.insert(id, pem.clone());
                cx.spawn(async move |_, _| {
                    if let Err(error) = vault.set_host_key(id, Some(&pem)).await {
                        log::warn!("保存内嵌私钥失败: {error}");
                    }
                })
                .detach();
            }
            return Some(tr!(cx, "（私钥已加密入库）").to_string());
        }
        if was_inline {
            let id = host.id;
            self.inline_keys.remove(&id);
            if let (Some(vault), true) = (self.vault.clone(), id != 0) {
                cx.spawn(async move |_, _| {
                    if let Err(error) = vault.set_host_key(id, None).await {
                        log::warn!("清除内嵌私钥失败: {error}");
                    }
                })
                .detach();
            }
            return Some(tr!(cx, "（已移除内嵌私钥）").to_string());
        }
        None
    }

    /// 选择私钥文件并读入表单（只在保存时写入加密库，读取后原文仅存内存）。
    ///
    /// 导入即做一次格式校验：能解析或"是加密私钥"才收下，避免把随便一个文本
    /// 存进库、等到连接时才失败。
    pub(super) fn import_inline_key(&mut self, cx: &mut Context<Self>) {
        if self.vault.is_none() {
            self.status = tr!(cx, "内嵌私钥需要加密凭据库（当前不可用）");
            cx.notify();
            return;
        }
        if self.new_host.is_none() {
            return;
        }
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!(cx, "选择私钥文件")),
        });
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.first().cloned() else {
                return;
            };
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            // 先按文件大小预检再整读：误选 ISO / 视频等大文件时，不至于把
            // 几 GB 先同步读进内存再做 64 KB 上限判定。
            let read = match std::fs::metadata(&path) {
                Ok(metadata) if metadata.len() > MAX_INLINE_KEY_BYTES as u64 => Err(
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "文件过大"),
                ),
                _ => std::fs::read_to_string(&path),
            };
            let _ = cx.with_window(entity_id, move |_, cx| {
                entity.update(cx, move |this, cx| {
                    this.apply_imported_key(label, read, cx);
                });
            });
        })
        .detach();
    }

    /// 校验并暂存导入的私钥（UI 线程侧）。
    pub(super) fn apply_imported_key(
        &mut self,
        label: String,
        read: std::io::Result<String>,
        cx: &mut Context<Self>,
    ) {
        let text = match read {
            Ok(text) if text.len() > MAX_INLINE_KEY_BYTES => {
                self.status = tr!(cx, "私钥文件过大（超过 64 KB），已拒绝导入");
                cx.notify();
                return;
            }
            Ok(text) => text,
            Err(error) => {
                log::warn!("读取私钥 {label} 失败: {error}");
                self.status = tr!(
                    cx,
                    "读取私钥文件 {label} 失败：{error}",
                    label = label,
                    error = error
                );
                cx.notify();
                return;
            }
        };
        // 能解析、或是"加密私钥"才收下；其余内容当误选文件处理。
        if let Err(error) = russh::keys::decode_secret_key(&text, None)
            && !matches!(error, russh::keys::Error::KeyIsEncrypted)
        {
            log::warn!("导入私钥失败（{label}）: {error}");
            self.status = tr!(cx, "无法识别私钥文件 {label}", label = label);
            cx.notify();
            return;
        }
        let Some(form) = self.new_host.as_mut() else {
            return;
        };
        form.key_data = Some(text);
        form.key_label = Some(label);
        form.key_inline = true;
        self.status = tr!(cx, "私钥已读入，保存主机时写入加密凭据库");
        cx.notify();
    }

    /// 打开系统文件对话框选择私钥文件，回填表单"私钥路径"。
    pub(super) fn browse_key_path(&mut self, cx: &mut Context<Self>) {
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
            prompt: Some(tr!(cx, "选择私钥文件")),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.first()
            {
                let value = path.to_string_lossy().into_owned();
                let _ = cx.with_window(entity_id, move |window, cx| {
                    input.update(cx, |state, cx| {
                        state.set_value(value, window, cx);
                    });
                });
            }
        })
        .detach();
    }

    /// Ctrl+N / 菜单项：打开新建主机表单。
    pub(super) fn new_host(&mut self, _: &NewHost, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form(window, cx);
    }

    /// 新建/编辑主机：居中模态弹窗。遮罩只挡交互、点击不关闭（防误丢已填内容），
    /// 关闭仅走"取消"按钮或标题栏 ×。
    pub(super) fn render_form_dialog(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(form) = self.new_host.as_ref() else {
            return div().into_any_element();
        };
        // 默认窗口只有 720 高，字段一多表单就会顶出可视区。整卡最大高度跟着窗口走，
        // 字段区自己滚动，标题与底部按钮永远留在窗口内。
        let max_height = (window.viewport_size().height - px(48.0)).max(px(360.0));
        let fields_max_height = (max_height - px(160.0)).max(px(200.0));
        let methods = [
            AuthMethod::Agent,
            AuthMethod::IdentityFile,
            AuthMethod::Password,
        ];
        let selected = form.auth;
        let protocol = form.protocol;
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
                    .max_h(max_height)
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
                                        tr!(cx, "编辑主机")
                                    } else {
                                        tr!(cx, "新建主机")
                                    }),
                            )
                            .child(
                                Button::new("close-form")
                                    .label("×")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
                            ),
                    )
                    .child(
                        v_flex()
                            .id("host-form-fields")
                            .gap_2()
                            .max_h(fields_max_height)
                            .overflow_y_scrollbar()
                            .child({
                                // 协议分段：终端（SSH / Telnet）与远程桌面（RDP / VNC）共用同一张卡片。
                                // 切换时若端口仍是旧协议的默认值，顺带换成新协议默认端口。
                                let options = [
                                    ProtocolKind::Ssh,
                                    ProtocolKind::Telnet,
                                    ProtocolKind::Rdp,
                                    ProtocolKind::Vnc,
                                ];
                                v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child(tr!(cx, "协议")))
                                    .child(h_flex().gap_2().children(options.into_iter().map(
                                        |option| {
                                            let is_selected = form.protocol == option;
                                            div()
                                                .id(SharedString::from(format!(
                                                    "protocol-{}",
                                                    option.label()
                                                )))
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
                                                .text_color(if is_selected {
                                                    cx.theme().primary_foreground
                                                } else {
                                                    pal.text
                                                })
                                                .when(is_selected, |this| {
                                                    this.font_weight(FontWeight::BOLD)
                                                })
                                                .child(option.label())
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        let Some(form) = this.new_host.as_mut()
                                                        else {
                                                            return;
                                                        };
                                                        if form.protocol == option {
                                                            return;
                                                        }
                                                        let previous = form
                                                            .protocol
                                                            .default_port()
                                                            .to_string();
                                                        let next =
                                                            option.default_port().to_string();
                                                        let port_now = form
                                                            .port
                                                            .read(cx)
                                                            .value()
                                                            .trim()
                                                            .to_string();
                                                        form.protocol = option;
                                                        if port_now == previous {
                                                            form.port.update(cx, |state, cx| {
                                                                state.set_value(next, window, cx);
                                                            });
                                                        }
                                                        cx.notify();
                                                    },
                                                ))
                                        },
                                    )))
                            })
                            .child(labeled_field(tr!(cx, "展示名"), &form.name, &pal))
                            .child(labeled_field(tr!(cx, "主机地址"), &form.host, &pal))
                            .child(
                                h_flex()
                                    .gap_3()
                                    .child(div().flex_1().child(labeled_field(
                                        tr!(cx, "端口"),
                                        &form.port,
                                        &pal,
                                    )))
                                    // Telnet / VNC 的登录名在会话内交互输入，表单里没有对应字段。
                                    .when(
                                        matches!(protocol, ProtocolKind::Ssh | ProtocolKind::Rdp),
                                        |this| {
                                            this.child(div().flex_1().child(labeled_field(
                                                tr!(cx, "用户名"),
                                                &form.user,
                                                &pal,
                                            )))
                                        },
                                    ),
                            )
                            // 桌面协议密码：已保存的密码一律不回填显示（与「修改参数」一致）。
                            // 这里输入的是「新密码」，留空保存＝沿用已保存的密码。
                            .when(
                                matches!(protocol, ProtocolKind::Rdp | ProtocolKind::Vnc),
                                |this| {
                                    this.child(
                                        v_flex()
                                            .gap_1()
                                            .child(
                                                h_flex()
                                                    .w_full()
                                                    .items_center()
                                                    .justify_between()
                                                    .child(div().text_sm().child(tr!(cx, "密码")))
                                                    .child(
                                                        h_flex()
                                                            .id("save-desktop-password")
                                                            .gap_2()
                                                            .items_center()
                                                            .when(self.vault.is_some(), |this| {
                                                                this.cursor_pointer().on_click(
                                                                    cx.listener(|this, _, _, cx| {
                                                                        if let Some(form) =
                                                                            this.new_host.as_mut()
                                                                        {
                                                                            form.save_secret =
                                                                                !form.save_secret;
                                                                            cx.notify();
                                                                        }
                                                                    }),
                                                                )
                                                            })
                                                            .child(
                                                                div().text_xs().text_color(
                                                                    if self.vault.is_none() {
                                                                        pal.muted
                                                                    } else if save_checked {
                                                                        pal.text
                                                                    } else {
                                                                        pal.muted
                                                                    },
                                                                )
                                                                .child(if self.vault.is_some() {
                                                                    if save_checked {
                                                                        tr!(cx, "☑ 保存密码到加密库")
                                                                    } else {
                                                                        tr!(cx, "☐ 保存密码到加密库")
                                                                    }
                                                                } else {
                                                                    tr!(cx, "密码仅本次内存持有")
                                                                }),
                                                            ),
                                                    ),
                                            )
                                            .child(styled_input(
                                                Input::new(&form.password)
                                                    .content_type(InputContentType::Password)
                                                    .mask_toggle(),
                                                &pal,
                                            )),
                                    )
                                },
                            )
                            // 分组 / 跳板机并排一行：两项都是可选的下拉，各占一半宽度，
                            // 省下的竖向空间留给私钥字段。
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child({
                                        // 分组选择：新建时默认未分组（从分组右键进入则预选该组），
                                        // 编辑时预填原归属；保存即落到这里选中的组。
                                        let mut options: Vec<SharedString> =
                                            vec![SharedString::from(UNGROUPED)];
                                        options.extend(
                                            self.sidebar
                                                .names()
                                                .map(|name| SharedString::from(name.to_string())),
                                        );
                                        let selected = form
                                            .group
                                            .as_ref()
                                            .and_then(|group| {
                                                options.iter().position(|option| option == group)
                                            })
                                            .or(Some(0));
                                        let change_options = options.clone();
                                        let entity = cx.entity();
                                        div().flex_1().child(
                                            v_flex()
                                                .gap_1()
                                                .child(div().text_sm().child(tr!(cx, "分组")))
                                                .child(
                                                    Select::new(options)
                                                        .id("host-group")
                                                        .selected(selected)
                                                        .w_full()
                                                        .on_change(
                                                            move |index, _name, _window, cx| {
                                                                let group = if index == 0 {
                                                                    None
                                                                } else {
                                                                    change_options
                                                                        .get(index)
                                                                        .cloned()
                                                                };
                                                                entity.update(cx, |this, cx| {
                                                                    if let Some(form) =
                                                                        this.new_host.as_mut()
                                                                    {
                                                                        form.group = group;
                                                                        cx.notify();
                                                                    }
                                                                });
                                                            },
                                                        ),
                                                ),
                                        )
                                    })
                                    .when(protocol == ProtocolKind::Ssh, |this| {
                                        this.child({
                                            // 跳板机选择：只列出可作为跳板的主机（排除自己与已挂跳板机的主机，
                                            // 避免自环与多级链）。降级模式没有稳定的主机 id，只给「直连」。
                                            let self_id = form
                                                .editing
                                                .and_then(|id| {
                                                    self.hosts.iter().find(|host| host.id == id)
                                                })
                                                .map(|host| host.id);
                                            let mut options: Vec<SharedString> =
                                                vec![tr!(cx, "直连（无跳板机）")];
                                            let mut ids: Vec<Option<i64>> = vec![None];
                                            if self.vault.is_some() {
                                                for host in &self.hosts {
                                                    if Some(host.id) == self_id
                                                        || host.proxy_jump.is_some()
                                                    {
                                                        continue;
                                                    }
                                                    let label = if host.name.trim().is_empty() {
                                                        host.addr()
                                                    } else {
                                                        host.name.clone()
                                                    };
                                                    ids.push(Some(host.id));
                                                    options.push(tr!(
                                                        cx,
                                                        "{name}（{addr}）",
                                                        name = label,
                                                        addr = host.addr()
                                                    ));
                                                }
                                            }
                                            let selected = form
                                                .jump
                                                .and_then(|id| {
                                                    ids.iter().position(|item| *item == Some(id))
                                                })
                                                .or(Some(0));
                                            let change_ids = ids.clone();
                                            let entity = cx.entity();
                                            div().flex_1().child(
                                                v_flex()
                                                    .gap_1()
                                                    .child(div().text_sm().child(tr!(cx, "跳板机")))
                                                    .child(
                                                        Select::new(options)
                                                            .id("host-jump")
                                                            .selected(selected)
                                                            .w_full()
                                                            .on_change(
                                                                move |index, _name, _window, cx| {
                                                                    let jump = change_ids
                                                                        .get(index)
                                                                        .copied()
                                                                        .flatten();
                                                                    entity.update(
                                                                        cx,
                                                                        |this, cx| {
                                                                            if let Some(form) = this
                                                                                .new_host
                                                                                .as_mut()
                                                                            {
                                                                                form.jump = jump;
                                                                                cx.notify();
                                                                            }
                                                                        },
                                                                    );
                                                                },
                                                            ),
                                                    ),
                                            )
                                        })
                                    }),
                            )
                            .when(protocol == ProtocolKind::Ssh, |this| {
                                this.child(
                                    v_flex()
                                        .gap_1()
                                        .child(div().text_sm().child(tr!(cx, "认证方式")))
                                        .child(h_flex().gap_2().children(methods.into_iter().map(
                                            |method| {
                                                let auth_id = SharedString::from(format!(
                                                    "auth-{}",
                                                    auth_label(cx, method)
                                                ));
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
                                                    .when(is_selected, |this| {
                                                        this.font_weight(FontWeight::BOLD)
                                                    })
                                                    .child(auth_label(cx, method))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        if let Some(form) = this.new_host.as_mut() {
                                                            form.auth = method;
                                                        }
                                                        cx.notify();
                                                    }))
                                            },
                                        ))),
                                )
                            })
                            // 私钥区块只在「私钥」认证下渲染：常驻占位会白撑两百多像素，
                            // 默认窗口装不下。窗口更矮时由字段区自身滚动兜底。
                            .when(
                                protocol == ProtocolKind::Ssh
                                    && selected == AuthMethod::IdentityFile,
                                |this| {
                                    this.child(
                            v_flex()
                                .gap_2()
                            // 私钥来源：本地文件 / 内嵌加密库。两段切换不改变弹窗高度
                            // （下方行数一致：路径行 ↔ 导入行）。
                            .child({
                                let inline_available = self.vault.is_some();
                                let source_id =
                                    |label: &str| SharedString::from(format!("key-source-{label}"));
                                let seg = |label: &'static str, inline: bool| {
                                    let is_selected = form.key_inline == inline;
                                    let enabled = !inline || inline_available;
                                    div()
                                        .id(source_id(label))
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
                                        .text_color(if is_selected {
                                            cx.theme().primary_foreground
                                        } else if enabled {
                                            pal.text
                                        } else {
                                            pal.muted
                                        })
                                        .when(is_selected, |this| {
                                            this.font_weight(FontWeight::BOLD)
                                        })
                                        .child(crate::i18n::translate(cx, label, &[]))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if inline && this.vault.is_none() {
                                                this.status =
                                                    tr!(cx, "内嵌私钥需要加密凭据库（当前不可用）");
                                                cx.notify();
                                                return;
                                            }
                                            if let Some(form) = this.new_host.as_mut() {
                                                form.key_inline = inline;
                                            }
                                            cx.notify();
                                        }))
                                };
                                v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child(tr!(cx, "私钥来源")))
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(seg("本地文件", false))
                                            .child(seg("内嵌到加密库", true)),
                                    )
                            })
                            .when(!form.key_inline, |this| {
                                this.child(
                                    h_flex()
                                        .gap_2()
                                        .items_end()
                                        .child(div().flex_1().child(labeled_field(
                                            tr!(cx, "私钥路径"),
                                            &form.key_path,
                                            &pal,
                                        )))
                                        .child(
                                            Button::new("browse-key")
                                                .label(tr!(cx, "浏览…"))
                                                .ghost()
                                                .flex_shrink_0()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.browse_key_path(cx);
                                                })),
                                        ),
                                )
                            })
                            .when(form.key_inline, |this| {
                                // 已导入的私钥只显示文件名与字节数，界面里绝不出现内容。
                                let note = match (&form.key_label, &form.key_data) {
                                    (Some(name), Some(pem)) => tr!(
                                        cx,
                                        "待保存：{name}（{bytes} 字节）",
                                        name = name,
                                        bytes = pem.len()
                                    ),
                                    _ if form.editing.is_some() => {
                                        tr!(cx, "已内嵌在加密库（重新导入可替换）")
                                    }
                                    _ => tr!(cx, "尚未导入私钥"),
                                };
                                this.child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_sm()
                                                .text_color(pal.muted)
                                                .truncate()
                                                .child(note),
                                        )
                                        .child(
                                            Button::new("import-key")
                                                .label(tr!(cx, "导入私钥…"))
                                                .ghost()
                                                .flex_shrink_0()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.import_inline_key(cx);
                                                })),
                                        ),
                                )
                            })
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        // 「保存口令」勾选与标签同排，省下一整行高度；
                                        // 库不可用时只留"仅本次内存持有"的灰字提示。
                                        h_flex()
                                            .w_full()
                                            .items_center()
                                            .justify_between()
                                            .child(div().text_sm().child(tr!(cx, "私钥口令")))
                                            .child(
                                                h_flex()
                                                    .id("save-passphrase")
                                                    .gap_2()
                                                    .items_center()
                                                    .when(self.vault.is_some(), |this| {
                                                        this.cursor_pointer().on_click(
                                                            cx.listener(|this, _, _, cx| {
                                                                if let Some(form) =
                                                                    this.new_host.as_mut()
                                                                {
                                                                    form.save_secret =
                                                                        !form.save_secret;
                                                                    cx.notify();
                                                                }
                                                            }),
                                                        )
                                                    })
                                                    .child(div().text_xs().text_color(
                                                        if self.vault.is_none() {
                                                            pal.muted
                                                        } else if save_checked {
                                                            pal.text
                                                        } else {
                                                            pal.muted
                                                        },
                                                    ).child(
                                                        if self.vault.is_some() {
                                                            if save_checked {
                                                                tr!(cx, "☑ 保存口令到加密库")
                                                            } else {
                                                                tr!(cx, "☐ 保存口令到加密库")
                                                            }
                                                        } else {
                                                            tr!(cx, "口令仅本次内存持有")
                                                        },
                                                    )),
                                            ),
                                    )
                                    .child(styled_input(
                                        Input::new(&form.passphrase)
                                            // 口令长且没有回显机会，给就地显隐切换；掩码状态记在
                                            // 这个 InputState 上，表单关掉即重置。
                                            .content_type(InputContentType::Password)
                                            .mask_toggle(),
                                        &pal,
                                    )),
                            ),
                        )
                                },
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("save-host")
                                    .label(if editing {
                                        tr!(cx, "保存修改")
                                    } else {
                                        tr!(cx, "保存")
                                    })
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| this.save_form(cx))),
                            )
                            .child(
                                Button::new("cancel-host")
                                    .label(tr!(cx, "取消"))
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 分组偏好落库；加密库不可用时退回旧 settings.json。
    /// 把跳板机映射回填到主机列表。
    ///
    /// 只对加密库模式生效：降级模式下 `proxy_jump` 直接序列化在 sessions.json 里，
    /// 这里再按（可能全为 0 的）id 覆盖反而会把它抹掉。
    pub(super) fn apply_jumps(&mut self) {
        if self.vault.is_none() {
            return;
        }
        for host in &mut self.hosts {
            host.proxy_jump = self.jumps.jump_of(host.id);
        }
    }

    /// 记录主机与跳板机的绑定关系。
    ///
    /// 加密库模式写 `settings` 的 `jumps` 键；降级模式跳板机字段直接随
    /// sessions.json 落盘，这里不动（否则会按 id=0 覆盖成空）。
    pub(super) fn note_jump(&mut self, host: &HostConfig, cx: &mut Context<Self>) {
        if self.vault.is_none() || self.jumps.jump_of(host.id) == host.proxy_jump {
            return;
        }
        self.jumps.set(host.id, host.proxy_jump);
        self.persist_jumps(cx);
    }

    /// 跳板机映射落库；加密库不可用时退回旧 settings.json。
    pub(super) fn persist_jumps(&self, cx: &mut Context<Self>) {
        let json = self.jumps.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_jumps(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("jumps", &json).await {
                log::warn!("写入库内跳板机映射失败: {error}");
            }
        })
        .detach();
    }

    /// 把协议映射回填到主机列表（只对加密库模式生效，理由同 [`Self::apply_jumps`]）。
    pub(super) fn apply_protocols(&mut self) {
        if self.vault.is_none() {
            return;
        }
        for host in &mut self.hosts {
            host.protocol = self.protocols.protocol_of(host.id).unwrap_or_default();
        }
    }

    /// 记录某主机的协议选择；降级模式下协议已随 sessions.json 落盘，这里不动。
    pub(super) fn note_protocol(&mut self, host: &HostConfig, cx: &mut Context<Self>) {
        if self.vault.is_none()
            || self.protocols.protocol_of(host.id).unwrap_or_default() == host.protocol
        {
            return;
        }
        if self.protocols.set(host.id, host.protocol) {
            self.persist_protocols(cx);
        }
    }

    /// 协议映射落库；加密库不可用时退回旧 settings.json。
    pub(super) fn persist_protocols(&self, cx: &mut Context<Self>) {
        let json = self.protocols.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_protocols(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("protocols", &json).await {
                log::warn!("写入库内协议映射失败: {error}");
            }
        })
        .detach();
    }
}
