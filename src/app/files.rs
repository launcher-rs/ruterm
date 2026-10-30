//! //! 远端文件面板：目录浏览、上传 / 下载传输、新建 / 重命名 / 删除与表格渲染。

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context,
    ContextMenuExt, Disableable, DragMoveEvent, Entity,
    ExternalPaths, FontWeight, Icon, IconName, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, PopupMenuItem,
    Render, ScrollableElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, h_flex, px, v_flex,
};

use crate::sessions::HostConfig;
use crate::FileIcon;
use crate::tr;

use super::*;

/// 远端文件名洗净成本地可用的文件名：拒绝空名与相对目录，替换路径分隔符和
/// Windows 非法字符，避免下载时跳目录或建不出文件。
pub(super) fn safe_local_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    let mut safe = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            safe.push('_');
        } else {
            safe.push(ch);
        }
    }
    let safe = safe.trim_end_matches(['.', ' ']);
    if safe.is_empty() {
        None
    } else {
        Some(safe.to_string())
    }
}

/// 用系统默认程序打开本地文件（文件面板里的“打开”）。
pub(super) fn open_path_with_default(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::PCWSTR;

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // 谓词留空按文件关联选择默认动作（打开 / 编辑），不额外拼接命令。
        let _ = unsafe {
            ShellExecuteW(
                None,
                PCWSTR::null(),
                PCWSTR(wide.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

/// 文件列表固定列宽：表头与数据行共用，保证纵向对齐。
/// 是拖拽前的默认值；实际宽度存在每个标签的 [`FileBrowser::cols`] 里。
pub(super) const FILE_COL_KIND_W: f32 = 36.0;
pub(super) const FILE_COL_SIZE_W: f32 = 48.0;
pub(super) const FILE_COL_TIME_W: f32 = 74.0;
pub(super) const FILE_COL_PERM_W: f32 = 66.0;
/// 可拖拽列宽的上下限（名称列占剩余空间，不在这里）。
pub(super) const FILE_COL_MIN_W: f32 = 34.0;
pub(super) const FILE_COL_MAX_W: f32 = 180.0;
/// 行首图标 + 间距占位：让表头「名称」与行内文件名左对齐。
pub(super) const FILE_ROW_LEAD_W: f32 = 20.0;

/// 远端修改时间（Unix 秒，UTC）→ `MM-DD HH:MM`（面板窄，省掉年份）。
pub(super) fn format_remote_time_short(seconds: u32) -> String {
    let secs = u64::from(seconds);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (_, month, day) = civil_from_days(days);
    format!(
        "{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Howard Hinnant 的 `civil_from_days`：Unix 天数 → 公历年月日（无需时区库）。
pub(super) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 合并两条状态附注（保存成功时的括注）。
pub(super) fn join_notes(first: Option<String>, second: Option<String>) -> String {
    let mut joined = String::new();
    if let Some(first) = first {
        joined.push_str(&first);
    }
    if let Some(second) = second {
        joined.push_str(&second);
    }
    joined
}

/// 文件列表的排序依据。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FileSortKey {
    Name,
    Kind,
    Size,
    Modified,
    Permissions,
}

/// 列宽拖拽的载荷：`tab_id` 保证拖拽期间切换标签不会改错表。
#[derive(Clone, Copy)]
pub(super) struct ColumnDrag {
    pub(super) tab_id: usize,
    pub(super) col: usize,
}

/// 列宽拖拽起点：按下时的指针 x 与当时的列宽。
#[derive(Clone, Copy)]
pub(super) struct ColumnDragState {
    pub(super) tab_id: usize,
    pub(super) col: usize,
    pub(super) start_x: f32,
    pub(super) start_w: f32,
}

/// 列宽拖拽的空拖影：rgpui 要求拖拽载荷能渲染成实体。
pub(super) struct ColumnDragGhost;

impl Render for ColumnDragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 右侧文件面板的单个标签状态：当前目录、条目与操作反馈。
pub(super) struct FileBrowser {
    /// 登录主目录（首次打开面板时列这里）。
    pub(super) home: Option<String>,
    /// 当前目录绝对路径。
    pub(super) path: Option<String>,
    pub(super) entries: Vec<crate::remote_fs::Entry>,
    /// 正在等 SFTP 回目录列表。
    pub(super) loading: bool,
    /// 最近一次操作错误；成功加载后清空。
    pub(super) error: Option<SharedString>,
    /// 正在执行的上传 / 打开等操作（面板底部提示）。
    pub(super) busy: Option<SharedString>,
    /// 上传进度 (已传, 总量)。
    pub(super) progress: Option<(u64, u64)>,
    /// 是否跟随终端当前目录：开启后，远端 shell cd 时面板自动跳转。
    pub(super) follow: bool,
    /// 排序依据与方向；目录始终在前。
    pub(super) sort: FileSortKey,
    pub(super) sort_desc: bool,
    /// 类型 / 大小 / 修改时间 / 权限 四列宽度；名称列占剩余空间。
    pub(super) cols: [f32; 4],
}

impl Default for FileBrowser {
    fn default() -> Self {
        Self {
            home: None,
            path: None,
            entries: Vec::new(),
            loading: false,
            error: None,
            busy: None,
            progress: None,
            follow: false,
            sort: FileSortKey::Name,
            sort_desc: false,
            cols: [
                FILE_COL_KIND_W,
                FILE_COL_SIZE_W,
                FILE_COL_TIME_W,
                FILE_COL_PERM_W,
            ],
        }
    }
}

impl FileBrowser {
    /// 按当前排序状态重排条目：目录恒在前，其余按所选列比较，同值回落到名称。
    pub(super) fn sort_entries(&mut self) {
        let key = self.sort;
        let desc = self.sort_desc;
        self.entries.sort_by(|left, right| {
            let by_dir = right.is_dir.cmp(&left.is_dir);
            if by_dir != Ordering::Equal {
                return by_dir;
            }
            let by_key = match key {
                // 名称列作为主键参与升降序；其余列同值时再回落到名称（恒升序）。
                FileSortKey::Name => left
                    .name
                    .to_lowercase()
                    .cmp(&right.name.to_lowercase())
                    .then_with(|| left.name.cmp(&right.name)),
                FileSortKey::Kind => kind_rank(left).cmp(&kind_rank(right)),
                FileSortKey::Size => left.size.cmp(&right.size),
                FileSortKey::Modified => left.modified.cmp(&right.modified),
                FileSortKey::Permissions => left.permissions.cmp(&right.permissions),
            };
            let by_key = if desc { by_key.reverse() } else { by_key };
            by_key
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.name.cmp(&right.name))
        });
    }
}

/// 类型排序次序：目录（已单独置顶）< 链接 < 普通文件。
pub(super) fn kind_rank(entry: &crate::remote_fs::Entry) -> u8 {
    if entry.is_dir {
        0
    } else if entry.is_symlink {
        1
    } else {
        2
    }
}

/// 两台已保存主机是否完全重复（忽略主键与收藏状态）。
pub(super) fn same_saved_host(left: &HostConfig, right: &HostConfig) -> bool {
    left.name == right.name
        && left.host == right.host
        && left.port == right.port
        && left.user == right.user
        && left.auth == right.auth
        && left.key_path == right.key_path
        && left.key_inline == right.key_inline
        && left.proxy_jump == right.proxy_jump
}

/// 文件面板里的新建 / 重命名弹窗。
pub(super) struct FilePrompt {
    pub(super) tab_id: usize,
    /// 目标目录（新建落在这里，重命名也在这里换名）。
    pub(super) dir: String,
    /// 重命名时的原名称；None 表示新建目录。
    pub(super) renaming: Option<String>,
    pub(super) name: Entity<InputState>,
}

/// 文件面板的删除二次确认（远端删除不可撤销）。
pub(super) struct FileConfirm {
    pub(super) tab_id: usize,
    pub(super) path: String,
    pub(super) name: String,
    pub(super) is_dir: bool,
}

/// 下载保存对话框的起始目录：优先系统下载目录，缺失则退回当前目录。
pub(super) fn downloads_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    home.map(|home| home.join("Downloads"))
        .filter(|dir| dir.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

impl RuTermApp {
    /// 文件分区首次展示时列出登录主目录（跟随终端开启时优先用终端目录）。
    pub(super) fn ensure_files_shown(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.active_tab else {
            return;
        };
        let (need_load, initial) = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| {
                let need = tab.files.path.is_none();
                let initial = if tab.files.follow {
                    tab.terminal_cwd.clone()
                } else {
                    None
                };
                (need, initial)
            })
            .unwrap_or((false, None));
        if need_load {
            self.load_files(id, initial, cx);
        }
    }


    /// 列出某标签的远端目录；`None` 表示登录主目录。
    pub(super) fn load_files(&mut self, tab_id: usize, path: Option<String>, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.ended {
            tab.files.error = Some("会话已断开，请先重新连接".into());
            cx.notify();
            return;
        }
        let Some(sender) = tab.commands.clone() else {
            tab.files.error = Some("本地终端没有远端文件".into());
            cx.notify();
            return;
        };
        tab.files.loading = true;
        tab.files.error = None;
        cx.notify();

        let (reply, rx) = tokio::sync::oneshot::channel();
        if sender
            .send(crate::session::SessionCommand::FsList { path, reply })
            .is_err()
        {
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                tab.files.loading = false;
                tab.files.error = Some("会话已断开，无法读取远端目录".into());
            }
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            let outcome = match rx.await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("文件列表请求已中断")),
            };
            let _ = this.update(cx, |this, cx| {
                let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                    return;
                };
                tab.files.loading = false;
                match outcome {
                    Ok(listing) => {
                        if tab.files.home.is_none() {
                            tab.files.home = Some(listing.path.clone());
                        }
                        tab.files.path = Some(listing.path);
                        tab.files.entries = listing.entries;
                        tab.files.sort_entries();
                        tab.files.error = None;
                    }
                    Err(error) => {
                        tab.files.error = Some(crate::session::error_summary(&error).into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }


    /// 刷新当前标签的目录（面板工具栏）。
    pub(super) fn refresh_files(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let path = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.files.path.clone());
        self.load_files(tab_id, path, cx);
    }


    /// 进入上级目录。
    pub(super) fn files_up(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let path = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.files.path.as_deref())
            .and_then(crate::remote_fs::parent_of);
        if let Some(path) = path {
            self.load_files(tab_id, Some(path), cx);
        }
    }


    /// 点击路径栏：激活编辑模式，输入框预填当前路径。
    pub(super) fn start_path_edit(&mut self, tab_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.files.path.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(&current));
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.commit_path_edit(cx);
            }
        })
        .detach();
        self.path_edit = Some(input);
        cx.notify();
    }


    /// 回车提交路径：跳转到输入的目录。
    pub(super) fn commit_path_edit(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.path_edit.take() else {
            return;
        };
        let tab_id = self.active_tab;
        let target = input.read(cx).text().to_string().trim().to_string();
        if target.is_empty() {
            cx.notify();
            return;
        }
        if let Some(tab_id) = tab_id {
            self.load_files(tab_id, Some(target), cx);
        }
        cx.notify();
    }


    /// 双击条目：目录就进去，文件就下载到临时目录并用系统默认程序打开。
    pub(super) fn open_file_entry(&mut self, tab_id: usize, name: &str, is_dir: bool, cx: &mut Context<Self>) {
        let Some(path) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.files.path.clone())
        else {
            return;
        };
        let target = crate::remote_fs::child(&path, name);
        if is_dir {
            self.load_files(tab_id, Some(target), cx);
            return;
        }
        self.open_remote_file(tab_id, target, name, cx);
    }


    /// 下载到临时目录后交给系统默认程序（“打开”）。
    pub(super) fn open_remote_file(
        &mut self,
        tab_id: usize,
        remote: String,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(safe) = safe_local_name(name) else {
            self.file_error(tab_id, "文件名不合法，无法在本地打开", cx);
            return;
        };
        let dir = std::env::temp_dir()
            .join("ruterm-open")
            .join(tab_id.to_string());
        if let Err(error) = std::fs::create_dir_all(&dir) {
            self.file_error(tab_id, format!("无法创建临时目录：{error}"), cx);
            return;
        }
        let local = dir.join(safe);
        self.file_transfer(
            tab_id,
            crate::transfer::Direction::Download,
            local,
            remote,
            true,
            false,
            cx,
        );
    }


    /// 面板内的一次上传 / 下载：进度写进面板底部，完成后刷新或打开。
    pub(super) fn file_transfer(
        &mut self,
        tab_id: usize,
        direction: crate::transfer::Direction,
        local: PathBuf,
        remote: String,
        open_after: bool,
        reload: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.files.busy.is_some() {
            self.status = "已有文件操作进行中，请稍候".into();
            cx.notify();
            return;
        }
        let Some(sender) = tab.commands.clone() else {
            tab.files.error = Some("会话已断开，无法传输文件".into());
            cx.notify();
            return;
        };
        let name = local
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| remote.clone());
        tab.files.busy = Some(format!("正在{} {name}…", direction.label()).into());
        tab.files.progress = Some((0, 0));
        tab.files.error = None;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, u64)>();
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        let command = crate::session::SessionCommand::Transfer(crate::transfer::Task {
            direction,
            local: local.clone(),
            remote: remote.clone(),
            progress: progress_tx,
            reply: reply_tx,
        });
        if sender.send(command).is_err() {
            tab.files.busy = None;
            tab.files.progress = None;
            tab.files.error = Some("会话已断开，无法传输文件".into());
            cx.notify();
            return;
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = loop {
                tokio::select! {
                    result = &mut reply_rx => {
                        break match result {
                            Ok(Ok(summary)) => Ok(summary),
                            Ok(Err(error)) => Err(crate::session::error_summary(&error)),
                            Err(_) => Err("传输任务已中断".to_string()),
                        };
                    }
                    Some(progress) = progress_rx.recv() => {
                        let _ = this.update(cx, |this, cx| {
                            if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id)
                                && tab.files.busy.is_some()
                            {
                                tab.files.progress = Some(progress);
                                cx.notify();
                            }
                        });
                    }
                }
            };
            let failed = outcome.is_err();
            let _ = this.update(cx, |this, cx| {
                let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                    return;
                };
                tab.files.busy = None;
                tab.files.progress = None;
                match outcome {
                    Ok(summary) => {
                        let message = format!(
                            "{}完成 · {} · {:.1} 秒",
                            direction.label(),
                            crate::transfer::format_bytes(summary.bytes),
                            summary.elapsed.as_secs_f64()
                        );
                        this.status = message.into();
                        tab.files.error = None;
                    }
                    Err(error) => {
                        tab.files.error = Some(error.clone().into());
                        this.status = format!("{}失败：{error}", direction.label()).into();
                    }
                }
                cx.notify();
            });
            if open_after && !failed {
                open_path_with_default(&local);
            }
            if reload {
                let path = this
                    .update(cx, |this, _| {
                        this.tabs
                            .iter()
                            .find(|tab| tab.id == tab_id)
                            .and_then(|tab| tab.files.path.clone())
                    })
                    .ok()
                    .flatten();
                if let Some(path) = path {
                    let _ = this.update(cx, |this, cx| this.load_files(tab_id, Some(path), cx));
                }
            }
        })
        .detach();
    }


    /// 拖入本地文件：逐个上传到当前目录（目录项拖放时传它自己的路径）。
    pub(super) fn upload_paths(
        &mut self,
        tab_id: usize,
        dir: String,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.files.busy.is_some() {
            self.status = "已有文件操作进行中，请稍候".into();
            cx.notify();
            return;
        }
        let Some(sender) = tab.commands.clone() else {
            tab.files.error = Some("会话已断开，无法上传文件".into());
            cx.notify();
            return;
        };
        let mut files = Vec::new();
        let mut skipped = Vec::new();
        for path in paths {
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => files.push(path),
                Ok(_) => skipped.push(format!("{}（不是文件）", path.display())),
                Err(error) => skipped.push(format!("{}（{error}）", path.display())),
            }
        }
        if files.is_empty() {
            self.file_error(
                tab_id,
                format!("没有可上传的文件：{}", skipped.join("；")),
                cx,
            );
            return;
        }
        let total = files.len();
        let note = if skipped.is_empty() {
            String::new()
        } else {
            format!("，跳过 {} 项", skipped.len())
        };
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.files.busy = Some(format!("正在上传 {total} 个文件…").into());
            tab.files.progress = Some((0, 0));
            tab.files.error = None;
        }
        self.status = format!("正在上传 {total} 个文件…{note}").into();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let mut failed = Vec::new();
            for path in files {
                let Some(name) = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                else {
                    failed.push(format!("{}（没有文件名）", path.display()));
                    continue;
                };
                let remote = crate::remote_fs::child(&dir, &name);
                let (progress_tx, mut progress_rx) =
                    tokio::sync::mpsc::unbounded_channel::<(u64, u64)>();
                let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                let command = crate::session::SessionCommand::Transfer(crate::transfer::Task {
                    direction: crate::transfer::Direction::Upload,
                    local: path.clone(),
                    remote,
                    progress: progress_tx,
                    reply: reply_tx,
                });
                if sender.send(command).is_err() {
                    failed.push(format!("{name}（会话已断开）"));
                    break;
                }
                let _ = this.update(cx, |this, cx| {
                    if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                        tab.files.busy = Some(format!("正在上传 {name}…").into());
                        cx.notify();
                    }
                });
                let outcome = loop {
                    tokio::select! {
                        result = &mut reply_rx => {
                            break match result {
                                Ok(Ok(summary)) => Ok(summary),
                                Ok(Err(error)) => Err(crate::session::error_summary(&error)),
                                Err(_) => Err("传输任务已中断".to_string()),
                            };
                        }
                        Some(progress) = progress_rx.recv() => {
                            let _ = this.update(cx, |this, cx| {
                                if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id)
                                    && tab.files.busy.is_some()
                                {
                                    tab.files.progress = Some(progress);
                                    cx.notify();
                                }
                            });
                        }
                    }
                };
                if let Err(error) = outcome {
                    failed.push(format!("{name}（{error}）"));
                }
            }
            let message = if failed.is_empty() {
                format!("上传完成 · {total} 个文件")
            } else {
                format!("上传结束 · {} 失败：{}", failed.len(), failed.join("；"))
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                    tab.files.busy = None;
                    tab.files.progress = None;
                    tab.files.error = if failed.is_empty() {
                        None
                    } else {
                        Some(message.clone().into())
                    };
                }
                this.status = message.into();
                cx.notify();
            });
            let path = this
                .update(cx, |this, _| {
                    this.tabs
                        .iter()
                        .find(|tab| tab.id == tab_id)
                        .and_then(|tab| tab.files.path.clone())
                })
                .ok()
                .flatten();
            if let Some(path) = path {
                let _ = this.update(cx, |this, cx| this.load_files(tab_id, Some(path), cx));
            }
        })
        .detach();
    }


    /// 面板「上传」：选本地文件后直接传到当前目录（不再经过传输表单）。
    pub(super) fn browse_files_upload(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.files.busy.is_some() {
            self.status = "已有文件操作进行中，请稍候".into();
            cx.notify();
            return;
        }
        let Some(dir) = tab.files.path.clone() else {
            return;
        };
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("选择要上传的文件".into()),
        });
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            if paths.is_empty() {
                return;
            }
            let _ = cx.with_window(entity_id, move |_window, cx| {
                entity.update(cx, move |this, cx| {
                    this.upload_paths(tab_id, dir.clone(), paths, cx);
                });
            });
        })
        .detach();
    }


    /// 行菜单「下载到…」：选保存位置后把该远端文件拉到本地。
    pub(super) fn download_file(&mut self, tab_id: usize, name: String, cx: &mut Context<Self>) {
        let Some(dir) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.files.path.clone())
        else {
            return;
        };
        let remote = crate::remote_fs::child(&dir, &name);
        let start = downloads_dir();
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        let rx = cx.prompt_for_new_path(&start, Some(&name));
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let _ = cx.with_window(entity_id, move |_window, cx| {
                entity.update(cx, move |this, cx| {
                    this.start_download(tab_id, path, remote, cx);
                });
            });
        })
        .detach();
    }


    /// 单个文件下载：进度写在面板底栏，完成后提示结果。
    pub(super) fn start_download(
        &mut self,
        tab_id: usize,
        local: PathBuf,
        remote: String,
        cx: &mut Context<Self>,
    ) {
        let name = Path::new(&remote)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| remote.clone());
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.files.busy.is_some() {
            self.status = "已有文件操作进行中，请稍候".into();
            cx.notify();
            return;
        }
        let Some(sender) = tab.commands.clone() else {
            tab.files.error = Some("会话已断开，无法下载文件".into());
            cx.notify();
            return;
        };
        tab.files.busy = Some(format!("正在下载 {name}…").into());
        tab.files.progress = Some((0, 0));
        tab.files.error = None;
        self.status = format!("正在下载 {name}…").into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (progress_tx, mut progress_rx) =
                tokio::sync::mpsc::unbounded_channel::<(u64, u64)>();
            let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
            let command = crate::session::SessionCommand::Transfer(crate::transfer::Task {
                direction: crate::transfer::Direction::Download,
                local,
                remote,
                progress: progress_tx,
                reply: reply_tx,
            });
            if sender.send(command).is_err() {
                let message = "会话已断开，无法下载文件".to_string();
                let _ = this.update(cx, |this, cx| {
                    if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                        tab.files.busy = None;
                        tab.files.progress = None;
                        tab.files.error = Some(message.clone().into());
                    }
                    this.status = message.into();
                    cx.notify();
                });
                return;
            }
            let outcome = loop {
                tokio::select! {
                    result = &mut reply_rx => {
                        break match result {
                            Ok(Ok(summary)) => Ok(summary),
                            Ok(Err(error)) => Err(crate::session::error_summary(&error)),
                            Err(_) => Err("传输任务已中断".to_string()),
                        };
                    }
                    Some(progress) = progress_rx.recv() => {
                        let _ = this.update(cx, |this, cx| {
                            if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id)
                                && tab.files.busy.is_some()
                            {
                                tab.files.progress = Some(progress);
                                cx.notify();
                            }
                        });
                    }
                }
            };
            let (message, error) = match outcome {
                Ok(summary) => (
                    format!(
                        "下载完成 · {} · {:.1} 秒",
                        crate::transfer::format_bytes(summary.bytes),
                        summary.elapsed.as_secs_f64()
                    ),
                    None,
                ),
                Err(error) => (
                    format!("下载失败：{error}"),
                    Some(format!("下载失败：{error}")),
                ),
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                    tab.files.busy = None;
                    tab.files.progress = None;
                    tab.files.error = error.map(Into::into);
                }
                this.status = message.into();
                cx.notify();
            });
        })
        .detach();
    }


    /// 拖文件到终端：逐个用 lrzsz（`rz`）上传到 shell 当前目录。
    ///
    /// 与面板上传不同，落点由远端 shell 决定，这里不拼任何路径；
    /// 每个文件一个 `rz` 会话，失败后继续传下一个，最后汇总。
    pub(super) fn upload_via_terminal(&mut self, tab_id: usize, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.zmodem.is_some() {
            self.status = "已有 lrzsz 上传进行中，请稍候".into();
            cx.notify();
            return;
        }
        if tab.ended {
            self.status = "会话已断开，无法上传文件".into();
            cx.notify();
            return;
        }
        let Some(sender) = tab.commands.clone() else {
            self.status = "本地终端不支持 lrzsz 上传，请使用 SSH 会话".into();
            cx.notify();
            return;
        };
        let mut files = Vec::new();
        let mut skipped = Vec::new();
        for path in paths {
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => files.push(path),
                Ok(_) => skipped.push(format!("{}（不是文件）", path.display())),
                Err(error) => skipped.push(format!("{}（{error}）", path.display())),
            }
        }
        if files.is_empty() {
            self.status = format!("没有可上传的文件：{}", skipped.join("；")).into();
            cx.notify();
            return;
        }
        let total = files.len();
        let note = if skipped.is_empty() {
            String::new()
        } else {
            format!("，跳过 {} 项", skipped.len())
        };
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.zmodem = Some(format!("正在通过 lrzsz 上传 {total} 个文件…").into());
            tab.zmodem_progress = Some((0, 0));
        }
        self.status = format!("正在通过 lrzsz 上传 {total} 个文件…{note}").into();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let mut failed = Vec::new();
            for path in files {
                let Some(name) = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                else {
                    failed.push(format!("{}（没有文件名）", path.display()));
                    continue;
                };
                let (progress_tx, mut progress_rx) =
                    tokio::sync::mpsc::unbounded_channel::<(u64, u64)>();
                let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                if sender
                    .send(crate::session::SessionCommand::ZmodemUpload {
                        local: path.clone(),
                        name: name.clone(),
                        progress: progress_tx,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    failed.push(format!("{name}（会话已断开）"));
                    break;
                }
                let _ = this.update(cx, |this, cx| {
                    if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                        tab.zmodem = Some(format!("正在通过 lrzsz 上传 {name}…").into());
                        cx.notify();
                    }
                });
                let outcome = loop {
                    tokio::select! {
                        result = &mut reply_rx => {
                            break match result {
                                Ok(Ok(summary)) => Ok(summary),
                                Ok(Err(error)) => Err(crate::session::error_summary(&error)),
                                Err(_) => Err("上传任务已中断".to_string()),
                            };
                        }
                        Some(progress) = progress_rx.recv() => {
                            let _ = this.update(cx, |this, cx| {
                                if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id)
                                    && tab.zmodem.is_some()
                                {
                                    tab.zmodem_progress = Some(progress);
                                    cx.notify();
                                }
                            });
                        }
                    }
                };
                if let Err(error) = outcome {
                    failed.push(format!("{name}（{error}）"));
                }
            }
            let message = if failed.is_empty() {
                format!("lrzsz 上传完成 · {total} 个文件")
            } else {
                format!(
                    "lrzsz 上传结束 · {} 失败：{}",
                    failed.len(),
                    failed.join("；")
                )
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                    tab.zmodem = None;
                    tab.zmodem_progress = None;
                }
                this.status = message.into();
                cx.notify();
            });
        })
        .detach();
    }


    /// 打开文件面板的新建 / 重命名弹窗。
    pub(super) fn open_file_prompt(
        &mut self,
        tab_id: usize,
        dir: String,
        renaming: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let default = renaming.clone().unwrap_or_default();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(default)
                .placeholder(tr!(cx, "名称"))
        });
        name.update(cx, |state, cx| {
            state.focus(window, cx);
            let len = state.value().len();
            state.set_selected_range(0..len, cx);
        });
        cx.subscribe(&name, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                this.commit_file_prompt(cx);
            }
        })
        .detach();
        self.file_prompt = Some(FilePrompt {
            tab_id,
            dir,
            renaming,
            name,
        });
        cx.notify();
    }


    pub(super) fn cancel_file_prompt(&mut self, cx: &mut Context<Self>) {
        self.file_prompt = None;
        cx.notify();
    }


    /// 提交新建 / 重命名；成功后刷新当前目录。
    pub(super) fn commit_file_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.file_prompt.as_ref() else {
            return;
        };
        let name = prompt.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.status = "名称不能为空".into();
            cx.notify();
            return;
        }
        let tab_id = prompt.tab_id;
        let dir = prompt.dir.clone();
        let renaming = prompt.renaming.clone();
        let Some(sender) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.commands.clone())
        else {
            self.status = "会话已断开，无法修改远端文件".into();
            self.file_prompt = None;
            cx.notify();
            return;
        };
        self.file_prompt = None;
        let (reply, rx) = tokio::sync::oneshot::channel();
        let command = match &renaming {
            Some(from) => crate::session::SessionCommand::FsRename {
                dir: dir.clone(),
                from: from.clone(),
                to: name.clone(),
                reply,
            },
            None => crate::session::SessionCommand::FsMkdir {
                dir: dir.clone(),
                name: name.clone(),
                reply,
            },
        };
        if sender.send(command).is_err() {
            self.status = "会话已断开，无法修改远端文件".into();
            cx.notify();
            return;
        }
        let action = if renaming.is_some() {
            "重命名"
        } else {
            "创建目录"
        };
        self.status = format!("正在{action} {name}…").into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = match rx.await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("远端操作请求已中断")),
            };
            let _ = this.update(cx, |this, cx| match outcome {
                Ok(_) => {
                    this.status = format!("{action}完成：{name}").into();
                    this.file_error_clear(tab_id);
                }
                Err(error) => {
                    let message = format!("{action}失败：{}", crate::session::error_summary(&error));
                    this.status = message.clone().into();
                    this.file_error(tab_id, message, cx);
                }
            });
            let path = this
                .update(cx, |this, _| {
                    this.tabs
                        .iter()
                        .find(|tab| tab.id == tab_id)
                        .and_then(|tab| tab.files.path.clone())
                })
                .ok()
                .flatten();
            if let Some(path) = path {
                let _ = this.update(cx, |this, cx| this.load_files(tab_id, Some(path), cx));
            }
        })
        .detach();
    }


    /// 请求删除（先弹二次确认）。
    pub(super) fn ask_delete_file(
        &mut self,
        tab_id: usize,
        name: String,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.files.path.as_deref())
        else {
            return;
        };
        let path = crate::remote_fs::child(path, &name);
        self.file_confirm = Some(FileConfirm {
            tab_id,
            path,
            name,
            is_dir,
        });
        cx.notify();
    }


    pub(super) fn cancel_delete_file(&mut self, cx: &mut Context<Self>) {
        self.file_confirm = None;
        cx.notify();
    }


    pub(super) fn confirm_delete_file(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.file_confirm.take() else {
            return;
        };
        let Some(sender) = self
            .tabs
            .iter()
            .find(|tab| tab.id == confirm.tab_id)
            .and_then(|tab| tab.commands.clone())
        else {
            self.status = "会话已断开，无法删除远端文件".into();
            cx.notify();
            return;
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        if sender
            .send(crate::session::SessionCommand::FsDelete {
                path: confirm.path.clone(),
                reply,
            })
            .is_err()
        {
            self.status = "会话已断开，无法删除远端文件".into();
            cx.notify();
            return;
        }
        let tab_id = confirm.tab_id;
        let name = confirm.name;
        self.status = format!("正在删除 {name}…").into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = match rx.await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("删除请求已中断")),
            };
            let _ = this.update(cx, |this, cx| match outcome {
                Ok(()) => {
                    this.status = format!("已删除 {name}").into();
                    this.file_error_clear(tab_id);
                    let path = this
                        .tabs
                        .iter()
                        .find(|tab| tab.id == tab_id)
                        .and_then(|tab| tab.files.path.clone());
                    if let Some(path) = path {
                        this.load_files(tab_id, Some(path), cx);
                    }
                }
                Err(error) => {
                    let message = format!("删除失败：{}", crate::session::error_summary(&error));
                    this.status = message.clone().into();
                    this.file_error(tab_id, message, cx);
                }
            });
        })
        .detach();
    }


    /// 面板内的错误：写该标签的 files.error 并同步状态栏。
    pub(super) fn file_error(
        &mut self,
        tab_id: usize,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let message = message.into();
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.files.error = Some(message.clone());
        }
        self.status = message;
        cx.notify();
    }


    pub(super) fn file_error_clear(&mut self, tab_id: usize) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.files.error = None;
        }
    }


    /// 文件列表表头的一格：点击切换排序；已排序列高亮并显示 ↑ / ↓。
    ///
    /// `width` 为 `Some((列下标, 宽度))` 时右边缘带拖拽手柄；名称列传 `None`，占剩余宽度。
    pub(super) fn file_sort_header(
        &self,
        id: &'static str,
        label: SharedString,
        key: FileSortKey,
        width: Option<(usize, f32)>,
        right_align: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let state = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .map(|tab| (tab.files.sort, tab.files.sort_desc));
        let active = state.is_some_and(|(sort, _)| sort == key);
        let desc = state.is_some_and(|(sort, desc)| sort == key && desc);

        let text = if active {
            format!("{label}{}", if desc { "↓" } else { "↑" })
        } else {
            label.to_string()
        };
        let mut cell = div()
            .id(id)
            .flex_shrink_0()
            .text_xs()
            .cursor_pointer()
            .text_color(if active { pal.text } else { pal.muted })
            .hover(|style| style.text_color(pal.text))
            .when(right_align, |this| this.text_right());
        if width.is_none() {
            cell = cell.flex_1().min_w(px(48.0)).text_ellipsis();
        }
        let cell = cell
            .child(text)
            .on_click(cx.listener(move |this, _, _, cx| {
                let Some(tab_id) = this.active_tab else {
                    return;
                };
                let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                    return;
                };
                if tab.files.sort == key {
                    tab.files.sort_desc = !tab.files.sort_desc;
                } else {
                    tab.files.sort = key;
                    tab.files.sort_desc = false;
                }
                tab.files.sort_entries();
                cx.notify();
            }));
        let Some((col, width)) = width else {
            return cell.into_any_element();
        };
        let Some(tab_id) = self.active_tab else {
            return div()
                .w(px(width))
                .flex_shrink_0()
                .child(cell)
                .into_any_element();
        };
        // 手柄压在相邻间距上（不占布局宽度），悬停时露出细线提示可拖。
        let grip = div()
            .id(("file-col-grip", col))
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(-5.0))
            .w(px(10.0))
            .cursor_col_resize()
            .hover(|style| style.bg(pal.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, _| {
                    this.col_drag = Some(ColumnDragState {
                        tab_id,
                        col,
                        start_x: event.position.x.as_f32(),
                        start_w: width,
                    });
                }),
            )
            .on_drag(ColumnDrag { tab_id, col }, |_, _, _, cx| {
                cx.new(|_| ColumnDragGhost)
            });
        div()
            .relative()
            .flex_shrink_0()
            .w(px(width))
            .child(cell.w_full())
            .child(grip)
            .into_any_element()
    }


    /// 文件页：路径栏 + 目录列表 + 底部操作状态。
    ///
    /// 面板跟随活动标签；每个标签各自保留路径与列表，切换回来不重新拉取。
    /// 目录行和列表空白处都接受系统文件拖入，落到对应目录上传。
    pub(super) fn render_files_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let tab = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id));
        let tab_id = tab.map(|tab| tab.id);
        let remote_capable = tab.is_some_and(|tab| tab.commands.is_some());
        let ended = tab.is_some_and(|tab| tab.ended);
        let path = tab.and_then(|tab| tab.files.path.clone());
        let loading = tab.is_some_and(|tab| tab.files.loading);
        let error = tab.and_then(|tab| tab.files.error.clone());
        let busy = tab.and_then(|tab| tab.files.busy.clone());
        let progress = tab.and_then(|tab| tab.files.progress);
        let entries = tab.map(|tab| tab.files.entries.clone()).unwrap_or_default();
        let entity = cx.entity();
        let live = tab_id.filter(|_| remote_capable && !ended);
        let follow_on = tab.is_some_and(|tab| tab.files.follow);
        let cols = tab.map(|tab| tab.files.cols).unwrap_or([
            FILE_COL_KIND_W,
            FILE_COL_SIZE_W,
            FILE_COL_TIME_W,
            FILE_COL_PERM_W,
        ]);

        let up_enabled = path.as_deref().and_then(crate::remote_fs::parent_of).is_some();
        let path_edit = self.path_edit.clone();
        let mut path_row = h_flex()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .py_1()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(pal.border);
        if let Some(input) = path_edit {
            path_row = path_row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(styled_input(Input::new(&input).text_xs(), &pal)),
            );
        } else {
            let tab_id_for_edit = live;
            path_row = path_row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .id("files-path-click")
                    .cursor_pointer()
                    .text_ellipsis()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(path.clone().unwrap_or_else(|| "—".to_string()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(id) = tab_id_for_edit {
                            this.start_path_edit(id, window, cx);
                        }
                    })),
            );
        }
        if let Some(id) = live {
            path_row = path_row
                .child({
                    let up = Button::new("files-up")
                        .icon(IconName::ArrowUp)
                        .compact()
                        .ghost()
                        .tab_index(-1)
                        .tooltip(tr!(cx, "上级目录"));
                    if up_enabled {
                        up.on_click(cx.listener(move |this, _, _, cx| {
                            this.files_up(id, cx);
                        }))
                    } else {
                        // 已在根目录：保留按钮位置，弱化且不挂回调。
                        up.text_color(pal.muted)
                    }
                })
                .child(
                    Button::new("files-refresh")
                        .label("⟳")
                        .compact()
                        .ghost()
                        .tab_index(-1)
                        .tooltip(tr!(cx, "刷新目录"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.refresh_files(id, cx);
                        })),
                )
                .child(
                    Button::new("files-mkdir")
                        .icon(IconName::Plus)
                        .compact()
                        .ghost()
                        .tab_index(-1)
                        .tooltip(tr!(cx, "新建文件夹"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let Some(dir) = this
                                .tabs
                                .iter()
                                .find(|tab| tab.id == id)
                                .and_then(|tab| tab.files.path.clone())
                            else {
                                return;
                            };
                            this.open_file_prompt(id, dir, None, window, cx);
                        })),
                )
                .child(
                    // 有文件浏览器就不需要单独的传输表单：直接选本地文件传到当前目录。
                    Button::new("files-upload")
                        .icon(FileIcon::Upload)
                        .compact()
                        .ghost()
                        .tab_index(-1)
                        .tooltip(tr!(cx, "上传文件…"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.browse_files_upload(id, cx);
                        })),
                );
        }

        let dir = path.clone().unwrap_or_default();
        // 上一帧 paint 写下的落点；取走即清，本帧若仍悬停在落点上会被重新写入。
        let drop_hint = self.drop_hint.take();
        let count = entries.len();
        let total: u64 = entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.size)
            .sum();
        // 摘要三段用「 · 」串起来：译文只负责各自片段，拼接受语言影响小。
        let mut summary_parts: Vec<SharedString> = vec![tr!(cx, "{count} 个项目", count = count)];
        if total > 0 {
            summary_parts.push(SharedString::from(crate::transfer::format_bytes(total)));
        }
        if remote_capable && !ended {
            summary_parts.push(tr!(cx, "拖入文件即可上传"));
        }
        let summary = SharedString::from(
            summary_parts
                .iter()
                .map(|part| part.as_str())
                .collect::<Vec<_>>()
                .join(" · "),
        );

        let list_body: AnyElement = if !remote_capable {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(if tab.is_some() {
                            tr!(cx, "本地终端没有远端文件可浏览")
                        } else {
                            tr!(cx, "没有活动会话")
                        }),
                )
                .into_any_element()
        } else if ended {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(tr!(cx, "会话已断开，请先重新连接")),
                )
                .into_any_element()
        } else if loading && entries.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(tr!(cx, "正在读取远端目录…")),
                )
                .into_any_element()
        } else if entries.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(if error.is_some() {
                            tr!(cx, "目录读取失败")
                        } else {
                            tr!(cx, "空目录")
                        }),
                )
                .into_any_element()
        } else {
            v_flex()
                .py_1()
                .gap_1()
                .children(entries.into_iter().map(|entry| {
                    let name = entry.name.clone();
                    let is_dir = entry.is_dir;
                    let mtime = entry
                        .modified
                        .map(format_remote_time_short)
                        .unwrap_or_else(|| "—".to_string());
                    let size_text = if is_dir {
                        "—".to_string()
                    } else {
                        crate::transfer::format_bytes(entry.size)
                    };
                    let kind_text = crate::i18n::translate(cx, entry.kind_text(), &[]);
                    let perm_text = entry.permissions_text();
                    let row_id = SharedString::from(format!(
                        "file-row-{}-{}-{}",
                        tab_id.unwrap_or_default(),
                        if is_dir { "d" } else { "f" },
                        name
                    ));
                    let dir_row = crate::remote_fs::child(&dir, &name);
                    let mut row = h_flex()
                        .id(row_id)
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .items_center()
                        .rounded(px(4.0))
                        .hover(|style| style.bg(pal.card_hover))
                        .cursor_pointer();
                    if is_dir {
                        // 拖到文件夹行上＝上传到该文件夹；其余行交给父级列表处理。
                        let target = dir_row.clone();
                        let hint_target = dir_row.clone();
                        let hint_entity = entity.clone();
                        row = row
                            .drag_over::<ExternalPaths>(move |style, paths, _, cx| {
                                // 行比父容器后画，所以悬停在文件夹上时提示的是这个子目录。
                                hint_entity.update(cx, |this, _| {
                                    this.drop_hint.set(Some((
                                        hint_target.clone().into(),
                                        paths.paths().len(),
                                    )));
                                });
                                style.bg(pal.card_hover).border_1().border_color(pal.accent)
                            })
                            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                                let Some(id) = live else {
                                    return;
                                };
                                this.drop_hint.set(None);
                                this.upload_paths(id, target.clone(), paths.paths().to_vec(), cx);
                            }));
                    }
                    if let Some(id) = live {
                        let opening_name = name.clone();
                        row = row.on_double_click(cx.listener(move |this, _, _, cx| {
                            this.open_file_entry(id, &opening_name, is_dir, cx);
                        }));
                    }
                    let row = row
                        .child(
                            Icon::new(if is_dir {
                                IconName::FolderOpen
                            } else {
                                IconName::File
                            })
                            .small()
                            .flex_shrink_0()
                            .text_color(if is_dir {
                                pal.link
                            } else {
                                pal.muted
                            }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(48.0))
                                .text_ellipsis()
                                .text_sm()
                                .child(name.clone()),
                        )
                        .child(
                            div()
                                .w(px(cols[0]))
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(pal.muted)
                                .child(kind_text),
                        )
                        .child(
                            div()
                                .w(px(cols[1]))
                                .flex_shrink_0()
                                .text_xs()
                                .text_right()
                                .text_color(pal.muted)
                                .child(size_text),
                        )
                        .child(
                            div()
                                .w(px(cols[2]))
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(pal.muted)
                                .child(mtime),
                        )
                        .child(
                            div()
                                .w(px(cols[3]))
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(pal.muted)
                                .child(perm_text),
                        );
                    let Some(id) = live else {
                        return row.into_any_element();
                    };
                    let menu_entity = entity.clone();
                    // 右键菜单的构造闭包是 'static（只拿得到 Entity），取不到 cx，
                    // 所以词条在行渲染时先取好再移进闭包。
                    let open_label = if is_dir {
                        tr!(cx, "打开目录")
                    } else {
                        tr!(cx, "打开")
                    };
                    let download_label = tr!(cx, "下载到…");
                    let rename_label = tr!(cx, "重命名…");
                    let delete_label = tr!(cx, "删除");
                    let refresh_label = tr!(cx, "刷新");
                    row.context_menu(move |menu, _, _| {
                        let opening = menu_entity.clone();
                        let opening_name = name.clone();
                        let menu = menu.item(PopupMenuItem::new(open_label.clone()).on_click(
                            move |_, _, cx| {
                                opening.update(cx, |this, cx| {
                                    this.open_file_entry(id, &opening_name, is_dir, cx);
                                });
                            },
                        ));
                        let menu = if is_dir {
                            menu
                        } else {
                            let downloading = menu_entity.clone();
                            let download_name = name.clone();
                            menu.item(PopupMenuItem::new(download_label.clone()).on_click(
                                move |_, _, cx| {
                                    downloading.update(cx, |this, cx| {
                                        this.download_file(id, download_name.clone(), cx);
                                    });
                                },
                            ))
                        };
                        let renaming = menu_entity.clone();
                        let rename_name = name.clone();
                        let menu = menu.item(PopupMenuItem::new(rename_label.clone()).on_click(
                            move |_, window, cx| {
                                renaming.update(cx, |this, cx| {
                                    let Some(dir) = this
                                        .tabs
                                        .iter()
                                        .find(|tab| tab.id == id)
                                        .and_then(|tab| tab.files.path.clone())
                                    else {
                                        return;
                                    };
                                    this.open_file_prompt(
                                        id,
                                        dir,
                                        Some(rename_name.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            },
                        ));
                        let deleting = menu_entity.clone();
                        let delete_name = name.clone();
                        let menu = menu.item(
                            PopupMenuItem::new(delete_label.clone())
                                .danger(true)
                                .on_click(move |_, _, cx| {
                                    deleting.update(cx, |this, cx| {
                                        this.ask_delete_file(id, delete_name.clone(), is_dir, cx);
                                    });
                                }),
                        );
                        let refreshing = menu_entity.clone();
                        menu.separator()
                            .item(PopupMenuItem::new(refresh_label.clone()).on_click(
                                move |_, _, cx| {
                                    refreshing.update(cx, |this, cx| this.refresh_files(id, cx));
                                },
                            ))
                    })
                    .into_any_element()
                }))
                .into_any_element()
        };

        let hint_entity = entity.clone();
        let hint_dir = dir.clone();
        let list = div().id("file-list").flex_1().min_h_0().overflow_y_scrollbar();
        // 没有活的 SFTP 会话就不接拖放：落点提示得有目标目录才有意义。
        let list = if live.is_some() {
            list.drag_over::<ExternalPaths>(move |style, paths, _, cx| {
                hint_entity.update(cx, |this, _| {
                    this.drop_hint.set(Some((
                        hint_dir.clone().into(),
                        paths.paths().len(),
                    )));
                });
                style.bg(pal.card_hover).border_1().border_color(pal.accent)
            })
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                let Some(id) = live else {
                    return;
                };
                this.drop_hint.set(None);
                this.upload_paths(id, dir.clone(), paths.paths().to_vec(), cx);
            }))
        } else {
            list
        };
        let list = list.child(list_body);

        // 表头：列宽与行内固定列共用常量；点击列名排序，再点切换升降序。
        let head_labels = (
            tr!(cx, "名称"),
            tr!(cx, "类型"),
            tr!(cx, "大小"),
            tr!(cx, "修改时间"),
            tr!(cx, "权限"),
        );
        let head_row = h_flex()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .py_1()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(pal.border)
            .child(div().w(px(FILE_ROW_LEAD_W)).flex_shrink_0())
            .child(self.file_sort_header(
                "files-sort-name",
                head_labels.0.clone(),
                FileSortKey::Name,
                None,
                false,
                cx,
            ))
            .child(self.file_sort_header(
                "files-sort-kind",
                head_labels.1.clone(),
                FileSortKey::Kind,
                Some((0, cols[0])),
                false,
                cx,
            ))
            .child(self.file_sort_header(
                "files-sort-size",
                head_labels.2.clone(),
                FileSortKey::Size,
                Some((1, cols[1])),
                true,
                cx,
            ))
            .child(self.file_sort_header(
                "files-sort-time",
                head_labels.3.clone(),
                FileSortKey::Modified,
                Some((2, cols[2])),
                false,
                cx,
            ))
            .child(self.file_sort_header(
                "files-sort-perm",
                head_labels.4,
                FileSortKey::Permissions,
                Some((3, cols[3])),
                false,
                cx,
            ))
            // 拖拽列宽：监听整行，指针离开手柄也不中断；释放时清掉起点状态。
            .on_drag_move::<ColumnDrag>(cx.listener(
                |this, event: &DragMoveEvent<ColumnDrag>, _, cx| {
                    let drag = *event.drag(cx);
                    let Some(start) = this.col_drag else {
                        return;
                    };
                    if start.tab_id != drag.tab_id || start.col != drag.col {
                        return;
                    }
                    let width = (start.start_w + (event.event.position.x.as_f32() - start.start_x))
                        .clamp(FILE_COL_MIN_W, FILE_COL_MAX_W);
                    if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == drag.tab_id)
                        && (tab.files.cols[drag.col] - width).abs() > 0.5
                    {
                        tab.files.cols[drag.col] = width;
                        cx.notify();
                    }
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.col_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.col_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            );
        let head_row = head_row.into_any_element();

        let mut footer = v_flex()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .py_1()
            .gap_1()
            .border_t_1()
            .border_color(pal.border);
        if let Some(text) = busy {
            let (done, total) = progress.unwrap_or((0, 0));
            let fraction = if total > 0 {
                (done as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            footer = footer.child(
                v_flex()
                    .gap_1()
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
            );
        }
        if let Some(error) = error {
            footer = footer.child(div().text_xs().text_color(pal.danger).child(error));
        }
        // 底部状态栏：左侧摘要信息，右侧「定位」+「跟随」两个按钮（同一行）
        footer = footer.child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(div().text_xs().text_color(pal.muted).child(summary))
                .child({
                    // 优先使用 terminal_cwd（来自 cd/OSC7），回退到 files.home（SFTP 初始目录），
                    // 再回退到 None（主目录）。
                    let target_path = tab
                        .and_then(|tab| tab.terminal_cwd.clone())
                        .or_else(|| tab.and_then(|tab| tab.files.home.clone()));
                    let follow_tooltip = if follow_on {
                        tr!(cx, "跟随终端：开（点击关闭）")
                    } else {
                        tr!(cx, "跟随终端目录")
                    };
                    // 跟随开＝钉住的图钉；关＝同一图钉加斜杠（lucide 没有官方 off 版，按家族风格补的）。
                    let follow_icon = if follow_on {
                        FileIcon::MapPinned
                    } else {
                        FileIcon::MapPinnedOff
                    };
                    h_flex()
                        .gap_1()
                        .items_center()
                        // 定位：一次性跳到终端当前目录
                        .child(
                            Button::new("files-locate")
                                .icon(FileIcon::MapPin)
                                .compact()
                                .ghost()
                                .tab_index(-1)
                                .tooltip(tr!(cx, "定位到终端当前目录"))
                                .disabled(live.is_none())
                                .on_click(cx.listener({
                                    let target = target_path.clone();
                                    move |this, _, _, cx| {
                                        let Some(id) = live else { return };
                                        let current = this
                                            .tabs
                                            .iter()
                                            .find(|t| t.id == id)
                                            .and_then(|t| t.files.path.clone());
                                        if current.as_deref() != target.as_deref() {
                                            this.load_files(id, target.clone(), cx);
                                        } else {
                                            cx.notify();
                                        }
                                    }
                                })),
                        )
                        // 跟随开关：开＝钉住，关＝钉住加斜杠
                        .child(
                            Button::new("files-follow")
                                .icon(follow_icon)
                                .compact()
                                .ghost()
                                .tab_index(-1)
                                .selected(follow_on)
                                .tooltip(follow_tooltip)
                                .disabled(live.is_none())
                                .on_click(cx.listener({
                                    let target = target_path.clone();
                                    move |this, _, _, cx| {
                                        let Some(id) = live else { return };
                                        let turn_on = !follow_on;
                                        if let Some(tab) = this.tabs.iter_mut().find(|t| t.id == id)
                                        {
                                            tab.files.follow = turn_on;
                                        }
                                        // 开启跟随时立刻对齐到终端目录
                                        if turn_on {
                                            let current = this
                                                .tabs
                                                .iter()
                                                .find(|t| t.id == id)
                                                .and_then(|t| t.files.path.clone());
                                            if current.as_deref() != target.as_deref() {
                                                this.load_files(id, target.clone(), cx);
                                                return;
                                            }
                                        }
                                        cx.notify();
                                    }
                                })),
                        )
                }),
        );

        v_flex()
            .id("files-section")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(path_row)
            .child(head_row)
            .child(list)
            .child(footer)
            // 落点说清楚：拖到面板上时，就地浮一条「松开即上传到 <目录>」，
            // 目录跟着光标所在行变（文件夹行＝该子目录，其余＝当前目录）。
            .when_some(drop_hint, |this, (target, count)| {
                this.child(
                    h_flex()
                        .absolute()
                        .left_2()
                        .right_2()
                        .bottom(px(40.0))
                        .justify_center()
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .bg(pal.accent)
                        .text_color(cx.theme().primary_foreground)
                        .text_xs()
                        .child(tr!(
                            cx,
                            "松开即上传 {count} 个文件到 {dir}",
                            count = count,
                            dir = target
                        )),
                )
            })
            .into_any_element()
    }


    /// 文件面板的新建 / 重命名弹窗。
    pub(super) fn render_file_prompt(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(prompt) = self.file_prompt.as_ref() else {
            return div().into_any_element();
        };
        let renaming = prompt.renaming.clone();
        let title = match &renaming {
            Some(old) => tr!(cx, "重命名：{old}", old = old),
            None => tr!(cx, "新建文件夹"),
        };
        let submit_label = if renaming.is_some() {
            tr!(cx, "重命名")
        } else {
            tr!(cx, "创建")
        };
        let cancel_label = tr!(cx, "取消");
        let name_label = tr!(cx, "名称");
        let name_hint = tr!(cx, "名称不能包含 / 或 \\，也不能是 . 或 ..。");
        v_flex()
            .id("file-prompt-backdrop")
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
                    .id("file-prompt")
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
                                    .font_weight(FontWeight(600.0))
                                    .child(title),
                            )
                            .child(
                                Button::new("close-file-prompt")
                                    .label("×")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_file_prompt(cx);
                                    })),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child(name_label))
                            .child(styled_input(Input::new(&prompt.name), &pal)),
                    )
                    .child(div().text_xs().text_color(pal.muted).child(name_hint))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("commit-file-prompt")
                                    .label(submit_label)
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.commit_file_prompt(cx);
                                    })),
                            )
                            .child(
                                Button::new("cancel-file-prompt")
                                    .label(cancel_label)
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_file_prompt(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }


    /// 远端删除的二次确认（目录非空会失败，提示文案同步说明）。
    pub(super) fn render_file_confirm(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(confirm) = self.file_confirm.as_ref() else {
            return div().into_any_element();
        };
        let name = confirm.name.clone();
        let title = if confirm.is_dir {
            tr!(cx, "删除远端目录 {name}？", name = name)
        } else {
            tr!(cx, "删除远端文件 {name}？", name = name)
        };
        let warning = if confirm.is_dir {
            tr!(cx, "目录必须为空才能删除；此操作不可撤销。")
        } else {
            tr!(cx, "文件将被永久删除，此操作不可撤销。")
        };
        let cancel_label = tr!(cx, "取消");
        let delete_label = tr!(cx, "删除");
        v_flex()
            .id("file-delete-backdrop")
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
                    this.cancel_delete_file(cx);
                }),
            )
            .child(
                v_flex()
                    .id("file-delete-dialog")
                    .w(px(360.0))
                    .gap_3()
                    .p_4()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight(600.0))
                            .child(title),
                    )
                    .child(div().text_xs().text_color(pal.muted).child(warning))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("cancel-file-delete")
                                    .label(cancel_label)
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_delete_file(cx);
                                    })),
                            )
                            .child(
                                Button::new("confirm-file-delete")
                                    .label(delete_label)
                                    .danger()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_delete_file(cx);
                                    })),
                            ),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    ),
            )
            .into_any_element()
    }


}

#[cfg(test)]
mod tests {
    use super::{FileBrowser, FileSortKey, format_remote_time_short, safe_local_name};
    use crate::remote_fs;

    #[test]
    fn safe_local_name_keeps_basename_only() {
        assert_eq!(safe_local_name("a/b.txt").as_deref(), Some("a_b.txt"));
        assert_eq!(
            safe_local_name(r"a\b:c?.txt").as_deref(),
            Some("a_b_c_.txt")
        );
        assert_eq!(safe_local_name("中文.md").as_deref(), Some("中文.md"));
        assert_eq!(safe_local_name(".."), None);
        assert_eq!(safe_local_name("..."), None);
        assert_eq!(safe_local_name("   "), None);
    }

    #[test]
    fn remote_time_uses_utc() {
        assert_eq!(format_remote_time_short(0), "01-01 00:00");
        assert_eq!(format_remote_time_short(1_700_000_000), "11-14 22:13");
    }

    fn entry(name: &str, is_dir: bool, size: u64, modified: Option<u32>) -> remote_fs::Entry {
        remote_fs::Entry {
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size,
            modified,
            permissions: Some(0o644),
        }
    }

    #[test]
    fn file_sort_keeps_dirs_first_and_follows_key() {
        let mut files = FileBrowser::default();
        files.entries = vec![
            entry("zeta", true, 0, None),
            entry("Beta.txt", false, 20, Some(2)),
            entry("alpha.txt", false, 10, Some(3)),
        ];
        files.sort_entries();
        let names = |files: &FileBrowser| {
            files
                .entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>()
        };
        // 名称升序：目录在前，文件按小写名称排。
        assert_eq!(names(&files), ["zeta", "alpha.txt", "Beta.txt"]);
        files.sort_desc = true;
        files.sort_entries();
        // 名称降序只反转文件之间的顺序，目录仍在最前。
        assert_eq!(names(&files), ["zeta", "Beta.txt", "alpha.txt"]);

        files.sort = FileSortKey::Size;
        files.sort_desc = true;
        files.sort_entries();
        assert_eq!(names(&files), ["zeta", "Beta.txt", "alpha.txt"]);
        files.sort_desc = false;
        files.sort_entries();
        assert_eq!(names(&files), ["zeta", "alpha.txt", "Beta.txt"]);
    }
}
