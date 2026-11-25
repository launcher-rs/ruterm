//! 检查更新的 UI 编排：帮助菜单触发 → 组件检查 → 版本对话框 → 下载进度 → 安装重启。
//!
//! 版本比较 / 镜像回退 / 分块下载 / 解压落地全部由 `rgpui-extra` 负责，
//! 这里只管应用侧状态、文案与弹窗（与 rumark 的接法保持一致）。

use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use rgpui::{
    AnyView, AnyWindowHandle, App, AsyncApp, Checkbox, Context, Disableable, PromptLevel,
    ScrollableElement, Window,
};
use rgpui_extra::updater::ui::{
    UpdateChoice, UpdateDialogContent, UpdateDialogStrings, UpdatePhase, UpdateStatus,
    render_update_dialog,
};

use crate::paths::StorageMode;
use crate::update as updater;

use super::*;

/// 「设置 → 更新」里镜像探测的进度与结果。
#[derive(Default)]
pub(super) struct MirrorProbeState {
    pub(super) checking: bool,
    pub(super) results: Vec<updater::MirrorProbe>,
}

/// 更新对话框需要的数据（下载时据此选择资产）。
#[derive(Clone, Debug)]
pub(super) struct UpdateDialogState {
    pub(super) current: String,
    pub(super) remote: String,
    pub(super) published: String,
    pub(super) notes: String,
    pub(super) asset: Option<updater::ReleaseAsset>,
    /// 本地已暂存且版本匹配的更新版本号；`Some` 时可直接安装，无需重新下载。
    pub(super) staged_version: Option<String>,
}

impl RuTermApp {
    /// 「帮助 → 检查更新…」：手动检查，避免启动时悄悄联网。
    pub(super) fn check_for_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.update_status.phase != UpdatePhase::Idle {
            return;
        }
        self.update_status = UpdateStatus::checking();
        cx.notify();

        let configs = fallback_configs(&self.update_prefs);
        let staged_config = configs[0].clone();
        let weak = cx.entity().downgrade();
        let window_handle = window.window_handle();
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            let result = updater::run_blocking(move || {
                try_configs(configs, |config| {
                    updater::check(config, updater::CURRENT_VERSION)
                })
            })
            .await;
            // 检查结果已拿到：提示框会等待用户点击，先收起状态栏进度，
            // 免得「已是最新版本」弹窗旁边还挂着「正在检查更新…」。
            let _ = weak.update(cx, |this, cx| {
                this.update_status = UpdateStatus::default();
                cx.notify();
            });
            let ok = cx.update(|cx| tr!(cx, "确定").to_string());
            match result {
                Ok(check) if check.newer => {
                    let remote = check.remote.version_string();
                    // 上次下载后点了「稍后」，本地留着同版本暂存文件时可直接安装。
                    let staged_version = {
                        let dir = updates_dir();
                        updater::load_staged_update(&dir, &staged_config.executable_name, &remote)
                            .map(|_| remote.clone())
                    };
                    let state = UpdateDialogState {
                        current: check.current.version_string(),
                        remote,
                        published: if check.release.published_at.is_empty() {
                            "-".to_string()
                        } else {
                            check.release.published_at
                        },
                        notes: check.release.body,
                        asset: check.asset,
                        staged_version,
                    };
                    let _ = weak.update(cx, |this, cx| {
                        this.update_dialog = Some(state);
                        cx.notify();
                    });
                }
                Ok(_) => {
                    let (title, detail) = cx.update(|cx| {
                        (
                            tr!(cx, "已是最新版本").to_string(),
                            tr!(cx, "当前版本 {version}", version = updater::CURRENT_VERSION)
                                .to_string(),
                        )
                    });
                    show_prompt(cx, window_handle, &title, &detail, &[&ok]).await;
                }
                Err(error) => {
                    let title = cx.update(|cx| tr!(cx, "检查更新失败").to_string());
                    show_prompt(cx, window_handle, &title, &error.to_string(), &[&ok]).await;
                }
            }
        })
        .detach();
    }

    /// 更新对话框按钮：稍后 / 打开发布页 / 下载（已有暂存文件时直接安装）。
    pub(super) fn on_update_choice(
        &mut self,
        choice: UpdateChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.update_dialog.take() else {
            return;
        };
        cx.notify();
        match choice {
            UpdateChoice::Later => {}
            UpdateChoice::OpenRelease => {
                let config = updater::updater_config(false, Vec::new());
                cx.spawn(async move |_this, _cx: &mut AsyncApp| {
                    let _ =
                        updater::run_blocking(move || updater::open_release_page(&config)).await;
                })
                .detach();
            }
            UpdateChoice::Download => {
                let staged = dialog.staged_version.as_deref().and_then(|version| {
                    let config = updater::updater_config(false, Vec::new());
                    updater::load_staged_update(&updates_dir(), &config.executable_name, version)
                });
                if let Some(staged) = staged {
                    let weak = cx.entity().downgrade();
                    let handle = window.window_handle();
                    cx.spawn(async move |_this, cx: &mut AsyncApp| {
                        if !confirm_and_install(staged.path, handle, cx).await {
                            let _ = weak.update(cx, |this, cx| {
                                this.update_status = UpdateStatus::default();
                                cx.notify();
                            });
                        }
                    })
                    .detach();
                    return;
                }
                let Some(asset) = dialog.asset else {
                    let title = tr!(cx, "检查更新失败").to_string();
                    let config = updater::updater_config(false, Vec::new());
                    let detail = tr!(
                        cx,
                        "Release 中没有找到当前平台的安装包：{asset}",
                        asset = config.asset_name()
                    )
                    .to_string();
                    let ok = tr!(cx, "确定").to_string();
                    let handle = window.window_handle();
                    cx.spawn(async move |_this, cx: &mut AsyncApp| {
                        show_prompt(cx, handle, &title, &detail, &[&ok]).await;
                    })
                    .detach();
                    return;
                };
                self.start_update_download(asset, dialog.remote, window, cx);
            }
        }
    }

    /// 后台下载并把进度回传给状态栏；主线程只做定时取队列与界面刷新。
    fn start_update_download(
        &mut self,
        asset: updater::ReleaseAsset,
        version: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_status = UpdateStatus::downloading();
        cx.notify();

        let configs = fallback_configs(&self.update_prefs);
        let (tx, rx) = mpsc::channel::<updater::DownloadProgress>();

        let pump = cx.entity().downgrade();
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            loop {
                let (latest, disconnected) = drain_latest_progress(&rx);
                if let Some(progress) = latest {
                    let _ = pump.update(cx, |this, cx| {
                        // 迟到的进度不能覆盖「下载完成」状态。
                        if this.update_status.phase == UpdatePhase::Downloading {
                            this.update_status.progress = Some(progress);
                            cx.notify();
                        }
                    });
                }
                if disconnected {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
            }
        })
        .detach();

        let finish = cx.entity().downgrade();
        let handle = window.window_handle();
        let keepalive = tx.clone();
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            let result = updater::run_blocking(move || {
                try_configs(configs, |config| {
                    updater::download_and_stage(
                        config,
                        &asset,
                        &version,
                        &updates_dir(),
                        |progress| {
                            let _ = tx.send(progress);
                        },
                    )
                })
            })
            .await;
            drop(keepalive);

            let ok = cx.update(|cx| tr!(cx, "确定").to_string());
            match result {
                Ok(path) => {
                    let _ = finish.update(cx, |this, cx| {
                        this.update_status = UpdateStatus::downloaded();
                        cx.notify();
                    });
                    let _ = confirm_and_install(path, handle, cx).await;
                }
                Err(error) => {
                    let title = cx.update(|cx| tr!(cx, "下载更新失败").to_string());
                    // 失败弹窗会等待用户点击，先收状态栏，别挂着「正在下载更新」。
                    let _ = finish.update(cx, |this, cx| {
                        this.update_status = UpdateStatus::default();
                        cx.notify();
                    });
                    show_prompt(cx, handle, &title, &error.to_string(), &[&ok]).await;
                }
            }
            let _ = finish.update(cx, |this, cx| {
                this.update_status = UpdateStatus::default();
                cx.notify();
            });
        })
        .detach();
    }

    /// 更新对话框覆盖层（无待处理对话框时不挂）。
    pub(super) fn render_update_overlay(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<rgpui::AnyElement> {
        let dialog = self.update_dialog.as_ref()?;
        let strings = UpdateDialogStrings {
            title: tr!(cx, "发现新版本").to_string(),
            // 组件会把值拼在标签后，这里用空占位符去掉标签里的 {version}/{date}。
            current_label: tr!(cx, "当前版本 {version}", version = "").to_string(),
            new_label: tr!(cx, "新版本 {version}", version = "").to_string(),
            published_label: tr!(cx, "发布于 {date}", date = "").to_string(),
            notes_label: tr!(cx, "更新说明").to_string(),
            download: if dialog.staged_version.is_some() {
                tr!(cx, "安装已下载的版本").to_string()
            } else {
                tr!(cx, "下载并安装").to_string()
            },
            later: tr!(cx, "稍后").to_string(),
            open_release: tr!(cx, "打开发布页").to_string(),
            empty_notes: tr!(cx, "（无更新说明）").to_string(),
        };
        let content = UpdateDialogContent {
            current: dialog.current.clone(),
            remote: dialog.remote.clone(),
            published: dialog.published.clone(),
            notes: dialog.notes.clone(),
            notes_max_height: 220.0,
        };
        Some(render_update_dialog(
            cx,
            &strings,
            &content,
            Self::on_update_choice,
        ))
    }

    /// 库内镜像偏好晚到时，在打开设置弹窗前回填输入框（程序化回填不触发变更事件）。
    /// 用户正在编辑且尚未保存时不覆盖，避免丢掉输入内容。
    pub(super) fn sync_update_mirror_input(&self, window: &mut Window, cx: &mut App) {
        if self.update_mirrors_dirty {
            return;
        }
        let text = self.update_prefs.mirrors_text();
        self.update_mirror_input.update(cx, |input, cx| {
            if input.value().as_ref() != text.as_str() {
                input.set_value(text, window, cx);
            }
        });
    }

    /// 保存镜像前缀：一行一个，保存后把输入框规范化回「已保存」状态。
    pub(super) fn save_update_mirrors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.update_mirror_input.read(cx).value().to_string();
        let mut prefs = self.update_prefs.clone();
        prefs.set_mirrors_text(&text);
        let normalized = prefs.mirrors_text();
        self.update_mirror_input.update(cx, |input, cx| {
            if input.value().as_ref() != normalized.as_str() {
                input.set_value(normalized, window, cx);
            }
        });
        self.update_mirrors_dirty = false;
        self.set_update_prefs(prefs, cx);
    }

    /// 「检查镜像」：后台逐个探测候选镜像的 API 与安装包，结果写回设置页。
    pub(super) fn check_mirrors(&mut self, cx: &mut Context<Self>) {
        if self.mirror_probe.checking {
            return;
        }
        self.mirror_probe.checking = true;
        self.mirror_probe.results.clear();
        cx.notify();

        let config = updater_config(&self.update_prefs);
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            let results = updater::run_blocking(move || updater::probe_mirrors(&config)).await;
            let _ = weak.update(cx, |this, cx| {
                this.mirror_probe.checking = false;
                this.mirror_probe.results = results;
                cx.notify();
            });
        })
        .detach();
    }

    /// 镜像总开关。
    pub(super) fn set_use_mirrors(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let mut prefs = self.update_prefs.clone();
        prefs.use_mirrors = enabled;
        self.set_update_prefs(prefs, cx);
    }

    fn set_update_prefs(&mut self, prefs: UpdatePrefs, cx: &mut Context<Self>) {
        if prefs == self.update_prefs {
            return;
        }
        self.update_prefs = prefs;
        cx.notify();
        self.persist_update_prefs(cx);
    }

    /// 检查更新偏好落库；加密库不可用时退回旧 settings.json。
    fn persist_update_prefs(&self, cx: &mut Context<Self>) {
        let json = self.update_prefs.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_update(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("update", &json).await {
                log::warn!("写入库内检查更新偏好失败: {error}");
            }
        })
        .detach();
    }

    /// 「更新」偏好页：镜像总开关 + 自定义镜像前缀 + 手动检查更新。
    pub(super) fn render_update_prefs_tab(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let app = cx.entity();
        let use_mirrors = self.update_prefs.use_mirrors;
        v_flex()
            .id("settings-tab-update")
            .gap_2()
            .child(div().text_sm().text_color(pal.muted).child(tr!(
                cx,
                "检查更新优先走官方直连；直连失败且开启镜像时，才按自定义前缀 → 内置镜像回退。"
            )))
            .child(
                Checkbox::new("update-use-mirrors")
                    .label(tr!(cx, "启用下载镜像"))
                    .checked(use_mirrors)
                    .on_change(move |value, _window, cx| {
                        app.update(cx, |this, cx| this.set_use_mirrors(value, cx));
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(pal.muted)
                            .child(tr!(cx, "自定义镜像前缀（每行一个）")),
                    )
                    .child(
                        TextArea::new(&self.update_mirror_input)
                            .rows(4)
                            .w_full()
                            .text_sm()
                            .bg(pal.input_bg)
                            .text_color(pal.input_text)
                            .border_1()
                            .border_color(pal.input_border),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if self.update_mirrors_dirty {
                                        pal.danger
                                    } else {
                                        pal.muted
                                    })
                                    .child(if self.update_mirrors_dirty {
                                        tr!(cx, "有未保存的更改")
                                    } else {
                                        tr!(cx, "已保存")
                                    }),
                            )
                            .child(
                                Button::new("update-mirrors-save")
                                    .label(tr!(cx, "保存镜像前缀"))
                                    .primary()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_update_mirrors(window, cx);
                                    })),
                            ),
                    )
                    .child(div().text_xs().text_color(pal.muted).child(tr!(
                        cx,
                        "每行一个前缀，回车换行；镜像回退时按书写顺序尝试，留空则只用内置镜像。"
                    )))
                    .when(!self.mirror_probe.results.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .id("mirror-probe-results")
                                .h(px(76.0))
                                .gap_1()
                                .p_2()
                                .bg(pal.card_bg)
                                .border_1()
                                .border_color(pal.border)
                                .rounded(px(4.0))
                                .overflow_y_scrollbar()
                                .children(self.mirror_probe.results.iter().map(|probe| {
                                    let name = if probe.mirror.is_empty() {
                                        tr!(cx, "直连").to_string()
                                    } else {
                                        probe.mirror.clone()
                                    };
                                    let api = if probe.api_ok {
                                        tr!(cx, "API 正常")
                                    } else {
                                        tr!(cx, "API 失败")
                                    };
                                    let download = if probe.download_ok {
                                        tr!(cx, "下载正常")
                                    } else {
                                        tr!(cx, "下载失败")
                                    };
                                    let suffix = if probe.detail.is_empty() {
                                        String::new()
                                    } else {
                                        format!("（{}）", probe.detail)
                                    };
                                    div()
                                        .text_xs()
                                        .text_color(pal.muted)
                                        .child(format!("{name}：{api} / {download}{suffix}"))
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("update-check-mirrors")
                            .label(if self.mirror_probe.checking {
                                tr!(cx, "正在检查镜像…")
                            } else {
                                tr!(cx, "检查镜像…")
                            })
                            .secondary()
                            .outline()
                            .disabled(self.mirror_probe.checking)
                            .on_click(cx.listener(|this, _, _, cx| this.check_mirrors(cx))),
                    )
                    .child(
                        Button::new("update-check-now")
                            .label(tr!(cx, "检查更新…"))
                            .secondary()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| {
                                // 检查前先应用尚未保存的镜像，避免「改了却没生效」。
                                this.save_update_mirrors(window, cx);
                                this.check_for_updates(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

/// 镜像探测用的配置：把用户填的前缀都列出来，直连也一并探测。
fn updater_config(prefs: &UpdatePrefs) -> rgpui_extra::updater::Config {
    updater::updater_config(prefs.use_mirrors, prefs.custom_mirrors.clone())
}

/// 实际更新走的候选配置：官方直连永远排第一；只有直连失败且用户开启镜像，
/// 才追加一组走自定义前缀 → 内置镜像 → 直连的配置。
fn fallback_configs(prefs: &UpdatePrefs) -> Vec<rgpui_extra::updater::Config> {
    let mut configs = vec![updater::updater_config(false, Vec::new())];
    if prefs.use_mirrors {
        configs.push(updater::updater_config(true, prefs.custom_mirrors.clone()));
    }
    configs
}

/// 依次尝试候选配置，返回第一个成功结果；全失败时保留最后一个错误。
fn try_configs<T, F>(configs: Vec<rgpui_extra::updater::Config>, mut job: F) -> anyhow::Result<T>
where
    F: FnMut(&rgpui_extra::updater::Config) -> anyhow::Result<T>,
{
    let mut last_error = None;
    for config in &configs {
        match job(config) {
            Ok(value) => return Ok(value),
            Err(error) => {
                log::warn!("更新通道失败，尝试下一个：{error}");
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("没有可用的更新通道")))
}

/// 更新暂存目录跟数据目录走，便携模式下也随程序目录一起搬走。
fn updates_dir() -> PathBuf {
    StorageMode::detect().dir().join("updates")
}

/// 弹出简单提示并等待用户点击，返回按钮索引。
async fn show_prompt(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    title: &str,
    detail: &str,
    answers: &[&str],
) -> usize {
    let title = title.to_string();
    let detail = detail.to_string();
    let answers: Vec<String> = answers.iter().map(|s| s.to_string()).collect();
    let receiver = cx.update_window(handle, move |_view: AnyView, window, cx| {
        let refs: Vec<&str> = answers.iter().map(String::as_str).collect();
        window.prompt(PromptLevel::Info, &title, Some(&detail), &refs, cx)
    });
    match receiver {
        Ok(rx) => rx.await.unwrap_or(0),
        Err(_) => 0,
    }
}

/// 确认后启动替换 helper 并退出主程序；返回 true 表示安装已启动。
async fn confirm_and_install(path: PathBuf, handle: AnyWindowHandle, cx: &mut AsyncApp) -> bool {
    let (title, detail, install, cancel) = cx.update(|cx| {
        (
            tr!(cx, "安装更新").to_string(),
            tr!(
                cx,
                "新版本已下载。安装会关闭 RuTerm，替换完成后自动重启。确定安装吗？"
            )
            .to_string(),
            tr!(cx, "立即安装").to_string(),
            tr!(cx, "取消").to_string(),
        )
    });
    // 取消放索引 0：提示框异常关闭时兜底返回 0，默认不替换正在运行的程序。
    let answer = show_prompt(cx, handle, &title, &detail, &[&cancel, &install]).await;
    if answer != 1 {
        return false;
    }
    let current_exe =
        cx.update(|_| std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ruterm")));
    let options = updater::InstallOptions {
        current_exe,
        staged_exe: path,
        process_id: std::process::id(),
        // RuTerm 没有需要恢复的命令行文档；快速连接参数重启后不必重放。
        restart_args: Vec::new(),
    };
    match updater::spawn_install_helper(&options) {
        Ok(_) => {
            cx.update(|cx| cx.quit());
            true
        }
        Err(error) => {
            let (title, ok) = cx.update(|cx| {
                (
                    tr!(cx, "启动安装失败").to_string(),
                    tr!(cx, "确定").to_string(),
                )
            });
            show_prompt(cx, handle, &title, &error.to_string(), &[&ok]).await;
            false
        }
    }
}

/// 排空进度队列只保留最新一条，避免状态栏显示几十秒前的旧进度。
fn drain_latest_progress(
    rx: &mpsc::Receiver<updater::DownloadProgress>,
) -> (Option<updater::DownloadProgress>, bool) {
    let mut latest = None;
    let mut disconnected = false;
    loop {
        match rx.try_recv() {
            Ok(progress) => latest = Some(progress),
            Err(TryRecvError::Disconnected) => {
                disconnected = true;
                break;
            }
            Err(TryRecvError::Empty) => break,
        }
    }
    (latest, disconnected)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::drain_latest_progress;
    use crate::update::DownloadProgress;

    fn progress(downloaded: u64) -> DownloadProgress {
        DownloadProgress {
            downloaded,
            total: Some(100),
        }
    }

    #[test]
    fn drain_keeps_only_latest_progress() {
        let (tx, rx) = mpsc::channel();
        tx.send(progress(10)).unwrap();
        tx.send(progress(50)).unwrap();
        tx.send(progress(90)).unwrap();
        let (latest, disconnected) = drain_latest_progress(&rx);
        assert_eq!(latest.map(|p| p.downloaded), Some(90));
        assert!(!disconnected);
    }

    #[test]
    fn drain_reports_disconnect_after_final_progress() {
        let (tx, rx) = mpsc::channel();
        tx.send(progress(70)).unwrap();
        drop(tx);
        let (latest, disconnected) = drain_latest_progress(&rx);
        assert_eq!(latest.map(|p| p.downloaded), Some(70));
        assert!(disconnected);
    }
}
