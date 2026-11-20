//! //! 凭据库界面：解锁弹窗、锁定、主密码设置 / 更换 / 取消（rekey）与凭据库设置分页。

use std::sync::Arc;

use rgpui::input_ui::{Input, InputContentType, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::title_bar::TITLE_BAR_HEIGHT;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, Disableable, Entity, FontWeight,
    InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, Styled, Window, div,
    h_flex, px, v_flex,
};

use crate::paths::StorageMode;
use crate::sessions::HostConfig;
use crate::tr;
use crate::vault::Vault;

use super::*;

/// 便携/主密码库的解锁弹窗状态（主密码派生密钥，库文件可带往他机）。
pub(super) struct UnlockState {
    pub(super) passphrase: Entity<InputState>,
    /// 首次建库：需要二次输入确认主密码（口令错误将无法重建库）。
    pub(super) confirm: Entity<InputState>,
    pub(super) is_new: bool,
    pub(super) error: Option<SharedString>,
}

/// 新口令输入框的提示语：口令档下这次操作是"更换"。
pub(super) fn password_placeholder(passphrase_mode: bool) -> &'static str {
    if passphrase_mode {
        "新主密码"
    } else {
        "主密码（可不设置）"
    }
}

impl RuTermApp {
    /// 打开解锁弹窗（启动自动弹出；"退出应用"关闭后可经侧边栏 🔑 重新打开）。
    pub(super) fn open_unlock_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.unlock.is_some() {
            return;
        }
        let passphrase = cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "主密码")));
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "再次输入确认")));
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
    pub(super) fn unlock_passphrase(&mut self, cx: &mut Context<Self>) {
        if self.vault_opening || self.vault_maintenance || self.vault.is_some() {
            return;
        }
        if let Err(error) = crate::vault::recover_rekey(&self.mode) {
            self.status = format!("事务恢复失败，拒绝解锁：{error:#}").into();
            cx.notify();
            return;
        }
        let Some(state) = self.unlock.as_ref() else {
            return;
        };
        let ctx = &mut *cx;
        let password = state.passphrase.read(ctx).text().to_string();
        if state.is_new {
            let confirm = state.confirm.read(ctx).text().to_string();
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
        let generation = self.auth_gate.generation();
        self.vault_opening = true;
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
            let _ = this.update(cx, |this, cx| {
                this.vault_opening = false;
                if this.auth_gate.generation() != generation {
                    return;
                }
                match opened {
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
                }
            });
        })
        .detach();
    }

    /// 退出解锁弹窗（尚未解锁时保持降级 JSON 行为）。
    pub(super) fn dismiss_unlock(&mut self, cx: &mut Context<Self>) {
        self.auth_gate.revoke();
        self.pending_quick = None;
        self.unlock = None;
        self.status = "凭据库未解锁，SSH 连接已禁用".into();
        cx.notify();
    }

    /// 锁定凭据库：丢弃库句柄与内存中的主密钥，清空主机列表并回到解锁弹窗。
    /// 已建立的终端会话不受影响，但不再能读取保存的凭据。
    pub(super) fn lock_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_maintenance {
            self.status = "凭据库事务进行中，请等待完成后锁定".into();
            cx.notify();
            return;
        }
        self.auth_gate.revoke();
        self.vault = None;
        self.unlocked_key = None;
        self.password_cache.clear();
        self.inline_keys.clear();
        self.pending = None;
        self.pending_quick = None;
        self.new_host = None;
        for tab in &mut self.tabs {
            if matches!(tab.resume, Some(Resume::Ssh { .. })) {
                tab.resume = None;
                tab.auto_retry_pending = false;
            }
        }
        self.hosts.clear();
        self.settings = None;
        self.status = "凭据库已锁定，需主密码解锁".into();
        self.open_unlock_dialog(window, cx);
        cx.notify();
    }

    pub(super) fn lock_vault_action(
        &mut self,
        _: &crate::LockVault,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vault.is_some() && self.passphrase_backend {
            self.lock_vault(window, cx);
        }
    }

    /// 将当前加密库复制到 exe 旁的 data/，切换为便携模式并重启。
    pub(super) fn enable_portable_mode(&mut self, cx: &mut Context<Self>) {
        if self.mode.is_portable() {
            return;
        }
        let Some(vault) = self.vault.clone() else {
            self.status = "凭据库未解锁，无法迁移到便携模式".into();
            cx.notify();
            return;
        };
        let Some(hexkey) = self.unlocked_key.clone() else {
            self.status = "凭据库密钥不可用，无法迁移到便携模式".into();
            cx.notify();
            return;
        };
        let Some(portable_dir) = StorageMode::portable_dir() else {
            self.status = "无法确定程序所在目录，无法启用便携模式".into();
            cx.notify();
            return;
        };
        if portable_dir.exists() {
            self.status = SharedString::from(format!(
                "便携目录已存在：{}；请先备份并移走该目录后重试",
                portable_dir.display()
            ));
            cx.notify();
            return;
        }
        let source_mode = self.mode.clone();
        let runtime = vault.runtime();
        if let Some(state) = self.settings.as_mut() {
            state.busy = true;
            state.error = None;
            state.info = None;
        }
        self.status = "正在复制凭据库到便携目录，完成后应用会重启…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = async {
                std::fs::create_dir(&portable_dir)?;
                let copied = async {
                    let dump = vault.export_all().await?;
                    let staging = portable_dir.join("ruterm.db.new");
                    crate::vault::remove_db_bundle(&staging);
                    let fresh =
                        Vault::open_at(&staging.to_string_lossy(), hexkey, runtime.clone()).await?;
                    fresh.import_dump(&dump).await?;
                    drop(fresh);
                    crate::vault::rename_db_bundle(
                        &runtime,
                        staging,
                        portable_dir.join(crate::paths::DB_FILE),
                        100,
                    )
                    .await
                    .map_err(anyhow::Error::msg)?;
                    for name in [
                        crate::paths::SALT_FILE,
                        crate::paths::UNLOCK_FILE,
                        crate::paths::LOCALE_FILE,
                        crate::paths::THEME_FILE,
                    ] {
                        let from = source_mode.dir().join(name);
                        if from.is_file() {
                            std::fs::copy(&from, portable_dir.join(name))?;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if copied.is_err() {
                    let _ = std::fs::remove_dir_all(&portable_dir);
                }
                copied
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(state) = this.settings.as_mut() {
                    state.busy = false;
                }
                match result {
                    Ok(()) => {
                        this.status = "便携数据已准备好；请退出并重新启动 RuTerm 以切换模式".into();
                        if let Some(state) = this.settings.as_mut() {
                            state.info = Some(
                                "便携数据已复制。重新启动 RuTerm 后生效；原数据会保留。".into(),
                            );
                        }
                        cx.notify();
                        cx.quit();
                    }
                    Err(error) => {
                        let message = format!("启用便携模式失败：{error}");
                        this.status = SharedString::from(message.clone());
                        if let Some(state) = this.settings.as_mut() {
                            state.error = Some(SharedString::from(message));
                        }
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// 点击"保存主密码"或回车：设置 / 更换主密码。
    pub(super) fn apply_rekey(&mut self, cx: &mut Context<Self>) {
        self.submit_master_password(MasterPasswordAction::Set, cx);
    }

    /// 点击"取消主密码"：库密钥换回系统凭据管理器里的随机密钥，此后启动无需输入口令。
    pub(super) fn cancel_master_password(&mut self, cx: &mut Context<Self>) {
        self.submit_master_password(MasterPasswordAction::Cancel, cx);
    }

    /// 执行主密码设置 / 更换 / 取消：校验当前口令 → 求目标密钥（新口令派生或 keyring）
    /// → 建新库全量搬迁 → 旧库让位 → 新库就位 → 写盐与解锁标记 → 确认数据无损后清理备份。
    pub(super) fn submit_master_password(
        &mut self,
        action: MasterPasswordAction,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.settings.as_ref() else {
            return;
        };
        if state.busy {
            return;
        }
        let ctx = &mut *cx;
        let needs_current = state.current.is_some();
        let current = state
            .current
            .as_ref()
            .map(|input| input.read(ctx).text().to_string());
        let password = state
            .password
            .as_ref()
            .map(|input| input.read(ctx).text().to_string());
        let confirm = state
            .confirm
            .as_ref()
            .map(|input| input.read(ctx).text().to_string());
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
        self.vault_maintenance = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = async {
                let dump = vault.export_all().await?;
                let expected_hosts = dump.hosts.len();
                let salt_path = mode.salt_path();
                let previous_salt = std::fs::read(&salt_path).ok();
                // turso 无 rekey：新建加密库全量搬迁，再原子换名。
                let staging = mode.db_path().with_file_name("ruterm.db.new");
                // 上一轮失败可能留下带旧数据的暂存束（含 -wal），必须先整套清掉。
                crate::vault::remove_db_bundle(&staging);
                std::fs::create_dir_all(mode.dir())?;
                let staging_db = staging.to_string_lossy().to_string();
                let fresh = Vault::open_at(&staging_db, hexkey.clone(), runtime.clone()).await?;
                fresh.import_dump(&dump).await?;
                drop(fresh);
                drop(vault);
                let db_path = mode.db_path();
                // 动库之前先落事务日志：此后任何一步崩溃，启动时的 recover_rekey
                // 都能凭日志判断主库新旧并把盐 / 解锁标记配平。
                crate::vault::write_rekey_journal(
                    &mode,
                    new_salt.as_ref(),
                    previous_salt.as_deref(),
                    backend,
                )?;
                let mut backup: Option<std::path::PathBuf> = None;
                if db_path.exists() {
                    let candidate = db_path.with_file_name("ruterm.db.keyring-bak");
                    let candidate = if candidate.exists() {
                        // 备份名被占用时带时间戳另存，确保旧库不丢。
                        db_path.with_file_name(format!(
                            "ruterm.db.bak-{}",
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
                        // 旧库已整体回位：事务作废，清掉日志，免得下次启动多跑一次恢复。
                        crate::vault::clear_rekey_journal(&mode);
                        return Err(anyhow::anyhow!("新库就位失败（旧库已放回原处）: {error}"));
                    }
                    backup = Some(candidate);
                } else {
                    crate::vault::rename_db_bundle(&runtime, staging, db_path.clone(), 100)
                        .await
                        .map_err(anyhow::Error::msg)?;
                }
                // 新库已就位：此时才可以换盐、换标记。这两步任何一步失败都必须
                // 把旧库放回原处并还原旧盐——只还原盐是不够的：留在原地的是用
                // 新盐派生的密钥加密的新库，旧口令派生不出密钥，等于把用户锁死。
                let swapped = (|| {
                    if let Some(salt) = new_salt {
                        crate::vault::write_salt(&salt_path, &salt)
                            .map_err(|error| anyhow::anyhow!("写入新派生盐失败: {error}"))?;
                    }
                    crate::vault::write_unlock_marker(&mode, backend)
                        .map_err(|error| anyhow::anyhow!("写入解锁标记失败: {error}"))?;
                    Ok::<(), anyhow::Error>(())
                })();
                if let Err(error) = swapped {
                    match backup.take() {
                        // 有旧库：整套放回原处（与上方"新库就位失败"的回滚对称）。
                        Some(candidate) => {
                            if let Err(rollback) = crate::vault::rename_db_bundle(
                                &runtime,
                                candidate,
                                db_path.clone(),
                                100,
                            )
                            .await
                            {
                                log::error!("rekey 回滚旧库失败，请手工检查备份: {rollback}");
                            }
                        }
                        // 换库前本来就没有旧库：把新库整套撤下，回到"无库"原状。
                        None => crate::vault::remove_db_bundle(&db_path),
                    }
                    if let Some(previous) = previous_salt.as_deref() {
                        let _ = std::fs::write(&salt_path, previous);
                    }
                    // 事务已干净回滚：清掉日志，免得下次启动又触发一次恢复。
                    crate::vault::clear_rekey_journal(&mode);
                    return Err(anyhow::anyhow!("{error}（已回滚：旧库与旧口令恢复原状）"));
                }
                let reopened = Vault::open(&mode, hexkey.clone(), runtime.clone()).await?;
                let hosts = reopened.load_hosts().await?;
                // 事务全部走完：清掉启动恢复用的日志。
                crate::vault::clear_rekey_journal(&mode);
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
            let _ = this.update(cx, |this, cx| {
                // 换钥流程刚写过（或已回滚）unlock.txt：无论成败都从盘上刷新缓存。
                this.refresh_unlock_backend();
                match result {
                    Ok((vault, hosts, hexkey, kept_backup)) => {
                        this.vault_maintenance = false;
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
                                None => {
                                    SharedString::from(format!("{done}；旧库备份已清理，数据完整"))
                                }
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
                        this.vault_maintenance = false;
                        log::warn!("更改主密码失败: {error}");
                        if let Some(state) = this.settings.as_mut() {
                            state.busy = false;
                            state.error = Some(format!("更改失败：{error}").into());
                        }
                        this.restore_vault(cx);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// 换密钥失败后尽力恢复库句柄：keyring 档直接重开；口令档旧密钥只存在于
    /// 解锁输入中，交回设置里的解锁入口。
    pub(super) fn restore_vault(&mut self, cx: &mut Context<Self>) {
        if self.vault.is_some() {
            return;
        }
        // 调用点（换钥失败）刚刷新过缓存，这里直接读内存。
        if self.passphrase_backend {
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
    pub(super) async fn after_vault_open(
        vault: &Vault,
        mode: &StorageMode,
        keyring_hexkey: Option<&str>,
        json_hosts: &[HostConfig],
    ) -> anyhow::Result<Vec<HostConfig>> {
        if let Some(key) = keyring_hexkey {
            crate::vault::migrate_legacy(vault, mode, key).await?;
        } else if let Some(text) = crate::vault::read_legacy_known_hosts(
            &mode.legacy_path(crate::paths::LEGACY_KNOWN_HOSTS_FILE),
        )? {
            // 旧数据库密钥不可用也不能跳过已有信任记录。
            crate::vault::import_legacy_trust(vault, &text).await?;
        }
        let existing = vault.load_hosts().await?;
        if existing.is_empty() && !json_hosts.is_empty() {
            let mut imported = json_hosts.to_vec();
            for (index, host) in imported.iter_mut().enumerate() {
                host.id = index as i64 + 1;
            }
            vault.import_hosts(&imported).await?;
            // 协议不在 hosts 表列里：把 JSON 里的 Telnet 选择写进 settings 的
            // protocols 键，否则下次启动会全部按 SSH 处理。
            let protocols = ProtocolPrefs::from_hosts(&imported);
            if !protocols.protocols.is_empty() {
                let _ = vault.set_setting("protocols", &protocols.to_json()).await;
            }
            return Ok(imported);
        }
        Ok(existing)
    }

    /// 便携/主密码库的解锁（或首次建库）居中弹窗。
    pub(super) fn render_unlock_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.unlock.as_ref() else {
            return div().into_any_element();
        };
        let is_new = state.is_new;
        let title = if is_new {
            tr!(cx, "设置主密码")
        } else {
            tr!(cx, "解锁凭据库")
        };
        let hint = if is_new {
            tr!(
                cx,
                "便携模式首次使用：设置主密码以创建加密库。密码仅用于派生库密钥，不会保存到任何地方——一旦忘记无法恢复，请务必牢记。"
            )
        } else {
            tr!(cx, "输入主密码解锁凭据库")
        };
        let cancel_label = tr!(cx, "退出应用");
        let confirm_label = if is_new {
            tr!(cx, "创建并解锁")
        } else {
            tr!(cx, "解锁")
        };
        v_flex()
            .id("unlock-backdrop")
            .absolute()
            .top(TITLE_BAR_HEIGHT)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .items_center()
            .justify_center()
            .bg(rgpui::linear_gradient(
                135.0,
                rgpui::linear_color_stop(rgpui::hsla(0.617, 0.34, 0.13, 1.0), 0.0),
                rgpui::linear_color_stop(rgpui::hsla(0.589, 0.38, 0.23, 1.0), 1.0),
            ))
            .child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .size(px(520.0))
                    .rounded_full()
                    .bg(rgpui::radial_gradient(
                        0.5,
                        0.5,
                        0.5,
                        &[
                            rgpui::linear_color_stop(rgpui::hsla(0.569, 0.75, 0.56, 0.20), 0.0),
                            rgpui::linear_color_stop(rgpui::hsla(0.569, 0.75, 0.56, 0.0), 1.0),
                        ],
                    )),
            )
            .child(
                div()
                    .absolute()
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .size(px(560.0))
                    .rounded_full()
                    .bg(rgpui::radial_gradient(
                        0.5,
                        0.5,
                        0.5,
                        &[
                            rgpui::linear_color_stop(rgpui::hsla(0.514, 0.72, 0.50, 0.16), 0.0),
                            rgpui::linear_color_stop(rgpui::hsla(0.514, 0.72, 0.50, 0.0), 1.0),
                        ],
                    )),
            )
            .child(
                div()
                    .absolute()
                    .right(px(58.0))
                    .bottom(px(38.0))
                    .text_size(px(104.0))
                    .font_weight(rgpui::FontWeight(700.0))
                    .text_color(rgpui::hsla(0.528, 0.68, 0.78, 0.10))
                    .child(">_"),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(
                v_flex()
                    .id("unlock-dialog")
                    .w(px(440.0))
                    .gap_4()
                    .p_6()
                    .rounded(px(14.0))
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
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .size(px(48.0))
                                    .rounded(px(14.0))
                                    .bg(rgpui::linear_gradient(
                                        135.0,
                                        rgpui::linear_color_stop(
                                            rgpui::hsla(0.611, 0.88, 0.56, 1.0),
                                            0.0,
                                        ),
                                        rgpui::linear_color_stop(
                                            rgpui::hsla(0.522, 0.82, 0.48, 1.0),
                                            1.0,
                                        ),
                                    ))
                                    .items_center()
                                    .justify_center()
                                    .text_size(px(17.0))
                                    .font_weight(rgpui::FontWeight(700.0))
                                    .text_color(rgpui::hsla(0.0, 0.0, 1.0, 1.0))
                                    .child(">_"),
                            )
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(rgpui::FontWeight(600.0))
                                            .text_color(pal.muted)
                                            .child("RuTerm"),
                                    )
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(rgpui::FontWeight(650.0))
                                            .child(title),
                                    ),
                            ),
                    )
                    .child(div().h(px(1.0)).w_full().bg(pal.border))
                    .child(div().text_sm().text_color(pal.muted).child(hint))
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
                                    .label(cancel_label)
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
                                    .label(confirm_label)
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.unlock_passphrase(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 「凭据库与主密码」分页：库位置与解锁方式、主密码设置/更换/取消、一键锁定。
    pub(super) fn render_vault_tab(
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
            .child(div().text_sm().text_color(pal.muted).child(tr!(
                cx,
                "位置：{path}　解锁方式：{mode}",
                path = self.mode.dir().display(),
                mode = if passphrase_mode {
                    tr!(cx, "主密码（Argon2id 派生）")
                } else {
                    tr!(cx, "系统凭据管理器（无需输入）")
                }
            )))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_sm().text_color(pal.muted).child(tr!(cx, "存储模式")))
                    .child(div().text_sm().font_weight(FontWeight(600.0)).child(if self.mode.is_portable() {
                        tr!(cx, "便携模式")
                    } else {
                        tr!(cx, "全局模式")
                    })),
            )
            .child(div().text_xs().text_color(pal.muted).child(if self.mode.is_portable() {
                tr!(cx, "数据位于程序目录的 data 文件夹；复制程序与 data 文件夹即可迁移。")
            } else {
                tr!(cx, "启用后会将加密凭据库复制到程序目录的 data 文件夹，应用随后自动退出；重新启动后切换，原数据会保留。")
            }))
            .when(!self.mode.is_portable(), |this| {
                this.child(
                    Button::new("enable-portable-mode")
                        .label(tr!(cx, "迁移并退出"))
                        .primary()
                        .disabled(!unlocked || state.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.enable_portable_mode(cx))),
                )
            })
            .child(
                div()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(if passphrase_mode {
                        tr!(cx, "主密码只用于派生库密钥，不会写入任何文件；忘记主密码将无法恢复数据。每次启动需输入主密码，库文件可凭口令带往其他机器解锁。")
                    } else {
                        tr!(cx, "当前由系统凭据管理器免密解锁，开箱即用。建议设置主密码：库文件可凭口令带往其他机器，他人取到文件也无法读取；忘记主密码将无法恢复数据。")
                    }),
            )
            .when(!unlocked, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(pal.danger)
                        .child(tr!(cx, "凭据库未解锁，无法设置或更换主密码。")),
                )
            })
            .when(unlocked, |this| {
                let form = state.password.as_ref().zip(state.confirm.as_ref());
                match form {
                    Some((password, confirm)) => {
                        let this = this.child(
                            div()
                                .text_base()
                                .font_weight(FontWeight(600.0))
                                .child(if passphrase_mode {
                                    tr!(cx, "更换主密码")
                                } else {
                                    tr!(cx, "设置主密码（可选）")
                                }),
                        );
                        let this = match state.current.as_ref() {
                            Some(current) => this.child(styled_input(
                                Input::new(current).content_type(InputContentType::Password),
                                pal,
                            )),
                            None => this,
                        };
                        this.child(styled_input(
                            Input::new(password).content_type(InputContentType::Password),
                            pal,
                        ))
                        .child(styled_input(
                            Input::new(confirm).content_type(InputContentType::Password),
                            pal,
                        ))
                        .when_some(state.error.as_ref().cloned(), |this, error| {
                            this.child(div().text_sm().text_color(pal.danger).child(error))
                        })
                        .when(state.busy, |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(pal.muted)
                                    .child(tr!(cx, "正在新建加密库并全量搬迁，请稍候…")),
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
                                                .label(tr!(cx, "取消主密码"))
                                                .secondary()
                                                .outline()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cancel_master_password(cx);
                                                })),
                                        )
                                    })
                                    .child(
                                        Button::new("rekey-apply")
                                            .label(if passphrase_mode {
                                                tr!(cx, "更换主密码")
                                            } else {
                                                tr!(cx, "设置主密码")
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
                                            tr!(cx, "更换主密码")
                                        } else {
                                            tr!(cx, "设置主密码")
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
                            .label(tr!(cx, "锁定凭据库"))
                            .secondary()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.lock_vault(window, cx);
                            })),
                    ),
                )
            })
            .into_any_element()
    }
}
