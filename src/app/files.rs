//! //! 远端文件面板：目录浏览、上传 / 下载传输、新建 / 重命名 / 删除与表格渲染。

use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, ContextMenuExt, Disableable, DragMoveEvent,
    Entity, ExternalPaths, FontWeight, Icon, IconName, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, PopupMenuItem, Render, ScrollableElement,
    SharedString, StatefulInteractiveElement, Styled, Window, div, h_flex, px, v_flex,
};

use crate::FileIcon;
use crate::sessions::HostConfig;
use crate::tr;

use super::*;

/// 传输方向对应的状态栏图标：上传向上、下载向下。
fn transfer_icon(direction: crate::transfer::Direction) -> IconName {
    if matches!(direction, crate::transfer::Direction::Upload) {
        IconName::ArrowUp
    } else {
        IconName::ArrowDown
    }
}

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

/// 落点已存在时给出不冲突的路径（`name (1).ext`、`name (2).ext` …）：
/// 没有"保存对话框"兜底的直接落盘路径必须自带防覆盖。
pub(super) fn unique_local_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    for index in 1..1000u32 {
        let candidate = if extension.is_empty() {
            parent.join(format!("{stem} ({index})"))
        } else {
            parent.join(format!("{stem} ({index}).{extension}"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    // 999 个重名都不够用的情况（基本不可能）：宁可沿用原路径走覆盖，
    // 也不把一个看起来疯掉的文件名塞给用户。
    path
}

#[cfg(test)]
mod tests {
    use super::unique_local_path;

    #[test]
    fn unique_path_suffixes_existing_files() {
        let dir = std::env::temp_dir().join(format!("ruterm-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("a.txt");
        std::fs::write(&base, b"x").unwrap();
        let unique = unique_local_path(base.clone());
        assert_eq!(unique, dir.join("a (1).txt"));
        std::fs::write(&unique, b"x").unwrap();
        assert_eq!(unique_local_path(base), dir.join("a (2).txt"));
        // 不存在的路径原样返回。
        assert_eq!(unique_local_path(dir.join("b.txt")), dir.join("b.txt"));
        std::fs::remove_dir_all(&dir).ok();
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

/// 启动时清理上次进程遗留的「打开远端文件」临时目录：进程崩溃 / 断电走
/// 不到删除路径，`ruterm-open-*` 会永久残留在系统临时目录里。新进程刚
/// 启动时不可能还有属于本进程的副本目录，现存的全是残骸，整目录删除
/// 即可（对不存在的条目是无害 no-op）。
pub(super) fn cleanup_stale_open_copies() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with("ruterm-open-")
            && let Err(error) = std::fs::remove_dir_all(entry.path())
        {
            log::warn!("清理残留临时目录 {} 失败: {error}", entry.path().display());
        }
    }
}

/// 每份新副本独占随机目录；create（非 create_dir_all）不接受预先存在的目录。
fn create_open_copy(name: &str) -> std::io::Result<PathBuf> {
    let safe = safe_local_name(name)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "文件名不合法"))?;
    for _ in 0..16 {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| std::io::Error::other(format!("无法生成临时目录随机数：{error}")))?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("ruterm-open-{suffix}"));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt as _;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match builder.create(&dir) {
            Ok(()) => return Ok(dir.join(safe)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "无法分配独占临时目录",
    ))
}

/// 远端文件只能作为文本编辑器的数据参数；绝不按扩展名执行或退回默认关联。
fn open_remote_in_editor(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt as _;
        // 从系统 API 获取可信目录，不使用 PATH、SystemRoot 或文件关联。
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
        }
        let mut buffer = vec![0u16; 32768];
        let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if len == 0 || len as usize >= buffer.len() {
            return Err("无法获取 Windows 系统目录，未打开远端文件".to_string());
        }
        let editor = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len as usize]))
            .join("notepad.exe");
        std::process::Command::new(editor)
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("无法启动系统记事本：{error}"))
    }
    #[cfg(target_os = "macos")]
    {
        // open(1): -a 指定应用，-- 结束选项；路径是 TextEdit 的文档参数。
        std::process::Command::new("/usr/bin/open")
            .args(["-a", "TextEdit", "--"])
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("无法启动 TextEdit：{error}"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let mut errors = Vec::new();
        for editor in [
            "/usr/bin/gnome-text-editor",
            "/usr/bin/gedit",
            "/usr/bin/kate",
            "/usr/bin/mousepad",
            "/usr/bin/pluma",
            "/usr/bin/xed",
            "/usr/bin/leafpad",
        ] {
            match std::process::Command::new(editor)
                .arg("--")
                .arg(path)
                .spawn()
            {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => errors.push(format!("{editor}: {error}")),
            }
        }
        Err(format!(
            "无法启动文本编辑器，请安装 gnome-text-editor、gedit、Kate 或 Mousepad；副本保存在 {}。{}",
            path.display(),
            errors.join("；")
        ))
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
/// 进度回传给 UI 线程的最小间隔：传输任务可能每块一条进度消息，
/// 大文件 × 小分块时是成千上万条，主线程不该为每条跑一遍实体更新。
const PROGRESS_UI_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// 行首图标 + 间距占位：让表头「名称」与行内文件名左对齐。
pub(super) const FILE_ROW_LEAD_W: f32 = 20.0;
/// 面板实测宽度低于此值算「窄栏」：隐掉修改时间与权限两列（见 [`RuTermApp::pane_narrow`]）。
pub(super) const FILE_PANE_NARROW_W: f32 = 430.0;

/// 「打开后回传」的本地巡检间隔。编辑器保存往往分成多次写盘，要连续两轮
/// 看到同一个新标记才认为写完，避免把半截内容传到远端。
const OPEN_FILE_POLL: Duration = Duration::from_millis(800);

/// 由「打开」下载到本地的远端文件：本地保存后自动回传远端。
pub(super) struct OpenedRemote {
    pub(super) local: PathBuf,
    pub(super) remote: String,
    pub(super) name: String,
    /// 仅刚下载或收到成功回执时更新，绝不以排队 / 发起上传代替同步成功。
    pub(super) synchronized: FileStamp,
    /// 等待确认 / 等待空闲的版本，与正在上传的版本分离。
    pending: Option<FileStamp>,
    inflight: Option<FileStamp>,
    /// 用户选择「否」的版本；后续编辑仍需再次提示。
    ignored: Option<FileStamp>,
    /// 上一轮看到的新标记：连续两轮相同才认为写入结束。
    pub(super) candidate: Option<FileStamp>,
}

/// 本地文件的版本标记：长度 + 修改时间，任一变就当作改过。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct FileStamp {
    len: u64,
    modified: Option<(u64, u32)>,
}

impl OpenedRemote {
    fn new(local: PathBuf, remote: String, name: String, synchronized: FileStamp) -> Self {
        Self {
            local,
            remote,
            name,
            synchronized,
            pending: None,
            inflight: None,
            ignored: None,
            candidate: None,
        }
    }

    /// 稳定两轮才入队；已在排队或传输时不重复入队。
    pub(super) fn ready_to_upload(&mut self, stamp: FileStamp) -> bool {
        if self.pending.is_some() || self.inflight.is_some() {
            return false;
        }
        if stamp == self.synchronized || self.ignored == Some(stamp) {
            self.candidate = None;
            return false;
        }
        if self.candidate == Some(stamp) {
            return true;
        }
        self.candidate = Some(stamp);
        false
    }

    fn cancel_pending(&mut self, stamp: FileStamp) {
        if self.pending == Some(stamp) {
            self.pending = None;
            self.candidate = None;
        }
    }

    fn ignore_pending(&mut self, stamp: FileStamp) {
        if self.pending == Some(stamp) {
            self.ignored = Some(stamp);
            self.cancel_pending(stamp);
        }
    }

    /// 排队期间再有编辑，必须重新稳定 / 确认，不能把新内容当作旧版本发送。
    fn begin_upload(&mut self, stamp: FileStamp, current: Option<FileStamp>) -> bool {
        if self.pending != Some(stamp) || self.inflight.is_some() {
            return false;
        }
        self.cancel_pending(stamp);
        if current != Some(stamp) {
            return false;
        }
        self.inflight = Some(stamp);
        true
    }

    fn finish_upload(&mut self, stamp: FileStamp, succeeded: bool) -> bool {
        if self.inflight != Some(stamp) {
            return false;
        }
        self.inflight = None;
        self.candidate = None;
        if succeeded {
            // 确认的是发送前的标记，不是回执到达时可能已经再次编辑的版本。
            self.synchronized = stamp;
        }
        true
    }
}

/// 取本地文件当前标记；文件已不存在（用户删了临时副本）时返回 None。
pub(super) fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| (time.as_secs(), time.subsec_nanos()));
    Some(FileStamp {
        len: metadata.len(),
        modified,
    })
}

/// 巡检一轮：把本地副本已稳定修改的文件挑出来（每轮最多一个，其余下一轮继续）。
/// 临时副本被用户删掉的顺带清理。
pub(super) fn poll_opened_files(browser: &mut FileBrowser) -> Option<(PathBuf, String, String)> {
    let mut ready = None;
    browser.opened.retain_mut(|item| {
        let Some(stamp) = file_stamp(&item.local) else {
            return false;
        };
        if item.ready_to_upload(stamp) && ready.is_none() {
            item.pending = Some(stamp);
            item.candidate = None;
            ready = Some((item.local.clone(), item.remote.clone(), item.name.clone()));
        }
        true
    });
    ready
}

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

/// 文件表格的数据源：远端某个标签的 SFTP 目录，或本机磁盘目录。
///
/// 两套面板共用同一张表（表头、排序、列宽、单元格），差别只在数据住在哪、
/// 以及各自行菜单能动谁。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneTarget {
    Remote(usize),
    Local,
}

/// 列宽拖拽的载荷：`target` 保证拖拽期间切换标签 / 面板不会改错表。
#[derive(Clone, Copy)]
pub(super) struct ColumnDrag {
    pub(super) target: PaneTarget,
    pub(super) col: usize,
}

/// 列宽拖拽起点：按下时的指针 x 与当时的列宽。
#[derive(Clone, Copy)]
pub(super) struct ColumnDragState {
    pub(super) target: PaneTarget,
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
    /// 通过「打开」落到本地的远端文件：本地改动稳定后自动传回。
    pub(super) opened: Vec<OpenedRemote>,
    /// 本地改动巡检循环是否已启动（每个标签一条，标签消失时自行退出）。
    pub(super) watching: bool,
    /// 打开 / 回传操作的本地路径身份，防止旧副本的进度和回执污染新操作。
    active_transfer: Option<PathBuf>,
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
            opened: Vec::new(),
            watching: false,
            active_transfer: None,
        }
    }
}

impl FileBrowser {
    fn opened_mut(&mut self, local: &Path, remote: &str) -> Option<&mut OpenedRemote> {
        self.opened
            .iter_mut()
            .find(|item| item.local == local && item.remote == remote)
    }

    fn finish_upload(
        &mut self,
        local: &Path,
        remote: &str,
        stamp: FileStamp,
        succeeded: bool,
    ) -> bool {
        self.opened_mut(local, remote)
            .is_some_and(|item| item.finish_upload(stamp, succeeded))
    }

    /// 按当前排序状态重排条目。
    pub(super) fn sort_entries(&mut self) {
        sort_entries(&mut self.entries, self.sort, self.sort_desc);
    }
}

/// 排序规则本体：目录恒在前，其余按所选列比较，同值回落到名称。
/// 写成自由函数是因为远端与本机两套面板共用同一份规则。
pub(super) fn sort_entries(entries: &mut [crate::remote_fs::Entry], key: FileSortKey, desc: bool) {
    entries.sort_by(|left, right| {
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

/// 本地副本改动后等待用户决定是否回传的文件（一次弹窗确认一个）。
pub(super) struct FileUploadPrompt {
    pub(super) tab_id: usize,
    pub(super) local: PathBuf,
    pub(super) remote: String,
    pub(super) name: String,
    /// 提示对应的稳定版本；「否」只忽略此版本。
    stamp: FileStamp,
    /// 用户已选择「是」但标签正忙时，保留确认，不再重复询问。
    approved: bool,
    /// 展示用的连接地址（`user@host:port`），与 WindTerm 的提示一致。
    pub(super) server: SharedString,
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
    pub(super) fn load_files(
        &mut self,
        tab_id: usize,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) {
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
    ///
    /// 新框要自己抢焦点并全选——点路径栏的人是想换一条路径，不是想在末尾续字。
    pub(super) fn start_path_edit(
        &mut self,
        target: PaneTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.pane_path(target);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(&current));
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.commit_path_edit(cx);
            }
        })
        .detach();
        input.update(cx, |state, cx| {
            let len = state.text().len();
            state.set_selected_range(0..len, cx);
            state.focus(window, cx);
        });
        self.path_edit = Some((target, input));
        cx.notify();
    }

    /// 回车提交路径：跳转到输入的目录。
    pub(super) fn commit_path_edit(&mut self, cx: &mut Context<Self>) {
        let Some((target, input)) = self.path_edit.take() else {
            return;
        };
        let text = input.read(cx).text().to_string().trim().to_string();
        if text.is_empty() {
            cx.notify();
            return;
        }
        match target {
            PaneTarget::Remote(tab_id) => self.load_files(tab_id, Some(text), cx),
            PaneTarget::Local => self.local_load(Some(PathBuf::from(text)), cx),
        }
        cx.notify();
    }

    /// 双击条目：目录就进去，远端文件下载到独占目录并作为文本打开。
    pub(super) fn open_file_entry(
        &mut self,
        tab_id: usize,
        name: &str,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
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

    /// 同一会话、完整远端路径复用副本；新副本的目录永不按文件名或标签共享。
    pub(super) fn open_remote_file(
        &mut self,
        tab_id: usize,
        remote: String,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.ended
            || tab
                .commands
                .as_ref()
                .is_none_or(|sender| sender.is_closed())
        {
            self.file_error(tab_id, "会话已断开，无法打开远端文件", cx);
            return;
        }
        // 重新下载会覆盖尚未回传的修改；只按完整远端路径复用。
        if let Some(opened) = tab
            .files
            .opened
            .iter()
            .find(|item| item.remote == remote && item.local.is_file())
        {
            if let Err(error) = open_remote_in_editor(&opened.local) {
                self.file_error(tab_id, error, cx);
            }
            return;
        }
        if tab.files.busy.is_some() {
            self.status = "已有文件操作进行中，请稍候".into();
            cx.notify();
            return;
        }
        let local = match create_open_copy(name) {
            Ok(local) => local,
            Err(error) => {
                self.file_error(tab_id, format!("无法创建临时副本：{error}"), cx);
                return;
            }
        };
        if !self.file_transfer(
            tab_id,
            crate::transfer::Direction::Download,
            local.clone(),
            remote,
            true,
            false,
            cx,
        ) && let Some(dir) = local.parent()
        {
            let _ = std::fs::remove_dir(dir);
        }
    }

    /// 登记一个已用本地程序打开的远端文件，并确保该标签的巡检循环在跑。
    pub(super) fn track_opened_remote(
        &mut self,
        tab_id: usize,
        local: PathBuf,
        remote: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(stamp) = file_stamp(&local) else {
            return;
        };
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        let Some(sender) = tab.commands.clone().filter(|sender| !sender.is_closed()) else {
            return;
        };
        if tab.ended {
            return;
        }
        if tab
            .files
            .opened
            .iter()
            .any(|item| item.remote == remote && item.local == local)
        {
            return;
        }
        // 新本地路径代表新副本，不能继承旧副本的「全部是」授权。
        self.file_upload_all.remove(&(tab_id, remote.clone()));
        // 移除旧记录前清掉它的临时副本目录，避免远端内容长期残留在 %TEMP%。
        let stale: Vec<PathBuf> = tab
            .files
            .opened
            .iter()
            .filter(|item| item.remote == remote && item.local != local)
            .filter_map(|item| item.local.parent().map(Path::to_path_buf))
            .collect();
        tab.files.opened.retain(|item| item.remote != remote);
        for dir in stale {
            let _ = std::fs::remove_dir_all(dir);
        }
        tab.files
            .opened
            .push(OpenedRemote::new(local, remote, name, stamp));
        if tab.files.watching {
            return;
        }
        tab.files.watching = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(OPEN_FILE_POLL).await;
                let watching = this
                    .update(cx, |this, cx| {
                        this.prune_remote_uploads(cx);
                        let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                            return false;
                        };
                        if !tab
                            .commands
                            .as_ref()
                            .is_some_and(|current| current.same_channel(&sender))
                        {
                            return false; // 旧巡检不能修改同 id 新会话的 watching。
                        }
                        if tab.ended || sender.is_closed() || tab.files.opened.is_empty() {
                            tab.files.watching = false;
                            return false;
                        }
                        // 忙时保留 pending，空闲后继续推进已确认的上传。
                        let ready = if tab.files.busy.is_none() {
                            poll_opened_files(&mut tab.files)
                        } else {
                            None
                        };
                        if let Some((local, remote, name)) = ready {
                            this.queue_remote_upload(tab_id, local, remote, name, cx);
                        }
                        this.pump_remote_uploads(cx);
                        true
                    })
                    .unwrap_or(false);
                if !watching {
                    break;
                }
            }
        })
        .detach();
    }

    /// 本地改动被确认后入队：默认先弹窗问用户，选过「全部是」的直接回传。
    pub(super) fn queue_remote_upload(
        &mut self,
        tab_id: usize,
        local: PathBuf,
        remote: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        let server = tab.addr.clone();
        let live = !tab.ended
            && tab
                .commands
                .as_ref()
                .is_some_and(|sender| !sender.is_closed());
        let Some(item) = tab.files.opened_mut(&local, &remote) else {
            return;
        };
        let Some(stamp) = item.pending else {
            return;
        };
        if !live {
            item.cancel_pending(stamp);
            self.file_upload_all.remove(&(tab_id, remote));
            return;
        }
        self.file_upload_queue.push(FileUploadPrompt {
            tab_id,
            local,
            remote,
            name,
            server,
            stamp,
            approved: false,
        });
        cx.notify();
    }

    fn upload_prompt_is_current(&self, prompt: &FileUploadPrompt) -> bool {
        self.tabs
            .iter()
            .find(|tab| tab.id == prompt.tab_id)
            .filter(|tab| {
                !tab.ended
                    && tab
                        .commands
                        .as_ref()
                        .is_some_and(|sender| !sender.is_closed())
            })
            .is_some_and(|tab| {
                tab.files.opened.iter().any(|item| {
                    item.local == prompt.local
                        && item.remote == prompt.remote
                        && item.pending == Some(prompt.stamp)
                })
            })
    }

    fn cancel_remote_upload(&mut self, prompt: &FileUploadPrompt) {
        if let Some(item) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == prompt.tab_id)
            .and_then(|tab| tab.files.opened_mut(&prompt.local, &prompt.remote))
        {
            item.cancel_pending(prompt.stamp);
            self.file_upload_all
                .remove(&(prompt.tab_id, prompt.remote.clone()));
        }
    }

    /// 关会话 / 换副本后清理确认与队列；仍存在的副本保留 synchronized，以免丢失 dirty。
    fn prune_remote_uploads(&mut self, cx: &mut Context<Self>) {
        if let Some(prompt) = self.file_upload_prompt.take() {
            if self.upload_prompt_is_current(&prompt) {
                self.file_upload_prompt = Some(prompt);
            } else {
                self.cancel_remote_upload(&prompt);
                cx.notify();
            }
        }
        for prompt in std::mem::take(&mut self.file_upload_queue) {
            if self.upload_prompt_is_current(&prompt) {
                self.file_upload_queue.push(prompt);
            } else {
                self.cancel_remote_upload(&prompt);
            }
        }
        let mut live = HashSet::new();
        for tab in &mut self.tabs {
            if tab.ended
                || tab
                    .commands
                    .as_ref()
                    .is_none_or(|sender| sender.is_closed())
            {
                for item in &mut tab.files.opened {
                    item.pending = None;
                    item.inflight = None;
                    item.candidate = None;
                }
                if tab.files.active_transfer.take().is_some() {
                    tab.files.busy = None;
                    tab.files.progress = None;
                    tab.files.error = Some("会话已断开，本地修改未回传".into());
                    cx.notify();
                }
            } else {
                live.extend(
                    tab.files
                        .opened
                        .iter()
                        .map(|item| (tab.id, item.remote.clone())),
                );
            }
        }
        self.file_upload_all.retain(|key| live.contains(key));
    }

    /// 推进回传队列：会话已消失的条目丢掉；开了「全部是」就直接传，
    /// 否则弹窗确认。目标标签正忙时先等着，下一轮巡检再推进。
    pub(super) fn pump_remote_uploads(&mut self, cx: &mut Context<Self>) {
        self.prune_remote_uploads(cx);
        if self.file_upload_prompt.is_some() {
            return;
        }
        let Some(front) = self.file_upload_queue.first() else {
            return;
        };
        if self
            .tabs
            .iter()
            .find(|tab| tab.id == front.tab_id)
            .is_none_or(|tab| tab.files.busy.is_some())
        {
            return;
        }
        if front.approved
            || self
                .file_upload_all
                .contains(&(front.tab_id, front.remote.clone()))
        {
            self.start_queued_upload(cx);
            return;
        }
        self.file_upload_prompt = Some(self.file_upload_queue.remove(0));
        cx.notify();
    }

    /// 忙时保留队首及其确认；文件变化则重新巡检，发送失败则撤销自动回传授权。
    fn start_queued_upload(&mut self, cx: &mut Context<Self>) {
        self.prune_remote_uploads(cx);
        let Some(front) = self.file_upload_queue.first() else {
            return;
        };
        if self
            .tabs
            .iter()
            .find(|tab| tab.id == front.tab_id)
            .is_none_or(|tab| tab.files.busy.is_some())
        {
            return;
        }
        let prompt = self.file_upload_queue.remove(0);
        let started = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == prompt.tab_id)
            .and_then(|tab| tab.files.opened_mut(&prompt.local, &prompt.remote))
            .is_some_and(|item| item.begin_upload(prompt.stamp, file_stamp(&prompt.local)));
        if !started {
            return;
        }
        self.status = format!("检测到 {} 已修改，正在回传远端…", prompt.name).into();
        if !self.file_transfer(
            prompt.tab_id,
            crate::transfer::Direction::Upload,
            prompt.local.clone(),
            prompt.remote.clone(),
            false,
            true,
            cx,
        ) {
            self.finish_remote_upload(
                prompt.tab_id,
                &prompt.local,
                &prompt.remote,
                prompt.stamp,
                false,
            );
        }
    }

    fn finish_remote_upload(
        &mut self,
        tab_id: usize,
        local: &Path,
        remote: &str,
        stamp: FileStamp,
        succeeded: bool,
    ) {
        let matched = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .is_some_and(|tab| tab.files.finish_upload(local, remote, stamp, succeeded));
        // 失败保留 dirty，但不允许「全部是」导致无限重试。再次提示后可重新授权。
        if matched && !succeeded {
            self.file_upload_all.remove(&(tab_id, remote.to_string()));
        }
    }

    /// 确认弹窗的回答：`upload` 表示回传当前文件，`all` 表示仅这份文件后续
    /// 改动不再询问（换文件仍弹窗）；两者都 false 即「否」，跳过这一份改动。
    pub(super) fn answer_remote_upload(&mut self, upload: bool, all: bool, cx: &mut Context<Self>) {
        let Some(mut prompt) = self.file_upload_prompt.take() else {
            return;
        };
        if !self.upload_prompt_is_current(&prompt) {
            self.cancel_remote_upload(&prompt);
        } else if upload {
            if all {
                self.file_upload_all
                    .insert((prompt.tab_id, prompt.remote.clone()));
            }
            prompt.approved = true;
            self.file_upload_queue.insert(0, prompt);
            self.start_queued_upload(cx);
        } else {
            if let Some(item) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == prompt.tab_id)
                .and_then(|tab| tab.files.opened_mut(&prompt.local, &prompt.remote))
            {
                item.ignore_pending(prompt.stamp);
            }
            self.file_upload_all.remove(&(prompt.tab_id, prompt.remote));
        }
        self.pump_remote_uploads(cx);
        cx.notify();
    }

    /// 面板内的一次上传 / 下载。false 表示未发送，调用方必须释放 inflight。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn file_transfer(
        &mut self,
        tab_id: usize,
        direction: crate::transfer::Direction,
        local: PathBuf,
        remote: String,
        open_after: bool,
        reload: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return false;
        };
        if tab.files.busy.is_some() {
            self.status = "已有文件操作进行中，请稍候".into();
            cx.notify();
            return false;
        }
        let Some(sender) = tab
            .commands
            .clone()
            .filter(|sender| !tab.ended && !sender.is_closed())
        else {
            tab.files.error = Some("会话已断开，无法传输文件".into());
            cx.notify();
            return false;
        };
        let upload_stamp = if matches!(direction, crate::transfer::Direction::Upload) {
            tab.files
                .opened_mut(&local, &remote)
                .and_then(|item| item.inflight)
        } else {
            None
        };
        let name = local
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| remote.clone());
        tab.files.busy = Some(format!("正在{} {name}…", direction.label()).into());
        tab.files.progress = Some((0, 0));
        tab.files.active_transfer = Some(local.clone());
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
            tab.files.active_transfer = None;
            tab.files.error = Some("会话已断开，无法传输文件".into());
            cx.notify();
            return false;
        }
        self.flash_transfer(
            format!("正在{} {name}…", direction.label()),
            transfer_icon(direction),
            false,
            cx,
        );
        cx.notify();

        cx.spawn(async move |this, cx| {
            let mut last_push: Option<std::time::Instant> = None;
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
                        // 大文件 × 小分块时进度消息成千上万条：不到 100ms
                        // 就不上 UI 线程，主线程不该为每条消息跑一遍 update。
                        let now = std::time::Instant::now();
                        if last_push.is_some_and(|last| now.duration_since(last) < PROGRESS_UI_INTERVAL) {
                            continue;
                        }
                        last_push = Some(now);
                        let _ = this.update(cx, |this, cx| {
                            if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id)
                                && !tab.ended
                                && tab.commands.as_ref().is_some_and(|current| current.same_channel(&sender))
                                && tab.files.active_transfer.as_ref() == Some(&local)
                            {
                                tab.files.progress = Some(progress);
                                cx.notify();
                            }
                        });
                    }
                }
            };
            let _ = this.update(cx, |this, cx| {
                // 校验、登记和打开处于同一次 UI 更新，关标签后不能在更新外启动编辑器。
                this.prune_remote_uploads(cx);
                let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id)
                    .filter(|tab| !tab.ended
                        && tab.commands.as_ref().is_some_and(|current| current.same_channel(&sender))
                        && tab.files.active_transfer.as_ref() == Some(&local))
                else {
                    return;
                };
                if let Some(stamp) = upload_stamp
                    && !tab.files.opened_mut(&local, &remote)
                        .is_some_and(|item| item.inflight == Some(stamp))
                    {
                        return;
                    }
                let succeeded = outcome.is_ok();
                let path = tab.files.path.clone();
                tab.files.busy = None;
                tab.files.progress = None;
                tab.files.active_transfer = None;
                let (message, icon) = match outcome {
                    Ok(summary) => {
                        tab.files.error = None;
                        (format!(
                            "{}完成 · {} · {:.1} 秒", direction.label(),
                            crate::transfer::format_bytes(summary.bytes), summary.elapsed.as_secs_f64()
                        ), IconName::CircleCheck)
                    }
                    Err(error) => {
                        tab.files.error = Some(error.clone().into());
                        (format!("{}失败：{error}", direction.label()), IconName::TriangleAlert)
                    }
                };
                if let Some(stamp) = upload_stamp {
                    this.finish_remote_upload(tab_id, &local, &remote, stamp, succeeded);
                }
                this.status = message.clone().into();
                this.flash_transfer(message, icon, true, cx);
                if open_after && succeeded {
                    let name = remote.rsplit('/').next().unwrap_or(&remote).to_string();
                    // 在启动编辑器之前记录下载版本，避免编辑器立即保存时被误记为已同步。
                    this.track_opened_remote(tab_id, local.clone(), remote.clone(), name, cx);
                    if let Err(error) = open_remote_in_editor(&local) {
                        this.file_error(tab_id, error, cx);
                    }
                }
                if reload && let Some(path) = path {
                    this.load_files(tab_id, Some(path), cx);
                }
                this.pump_remote_uploads(cx);
                cx.notify();
            });
        })
        .detach();
        true
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
        self.flash_transfer(
            format!("正在上传 {total} 个文件…{note}"),
            IconName::ArrowUp,
            false,
            cx,
        );
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
                let mut last_push: Option<std::time::Instant> = None;
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
                            // 进度消息节流到 100ms 一条（理由同单文件上传）。
                            let now = std::time::Instant::now();
                            if last_push.is_some_and(|last| now.duration_since(last) < PROGRESS_UI_INTERVAL) {
                                continue;
                            }
                            last_push = Some(now);
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
                this.status = message.clone().into();
                this.flash_transfer(
                    message,
                    if failed.is_empty() {
                        IconName::CircleCheck
                    } else {
                        IconName::TriangleAlert
                    },
                    true,
                    cx,
                );
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

    /// 行菜单「下载」：默认落到上次的下载目录，不再每个文件都弹保存对话框。
    ///
    /// 没记过目录（或记着的目录已被删）才弹对话框选位置，选完记下；同名文件
    /// 自动加序号——这条路没有对话框兜底，用户看不见"将要覆盖"这件事。
    pub(super) fn download_file(&mut self, tab_id: usize, name: String, cx: &mut Context<Self>) {
        let remembered = self.download_dir.clone().filter(|dir| dir.is_dir());
        let Some(dir) = remembered else {
            self.prompt_download_file(tab_id, name, cx);
            return;
        };
        let Some(remote_dir) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.files.path.clone())
        else {
            return;
        };
        let Some(safe) = safe_local_name(&name) else {
            self.file_error(tab_id, "文件名不合法，无法在本机保存", cx);
            return;
        };
        let remote = crate::remote_fs::child(&remote_dir, &name);
        let target = unique_local_path(dir.join(&safe));
        self.start_download(tab_id, target, remote, false, cx);
    }

    /// 行菜单「下载到…（换目录）」：弹保存对话框选位置和文件名，并把目录记成下次默认。
    pub(super) fn prompt_download_file(
        &mut self,
        tab_id: usize,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.files.path.clone())
        else {
            return;
        };
        let remote = crate::remote_fs::child(&dir, &name);
        let start = self
            .download_dir
            .clone()
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(downloads_dir);
        let entity = cx.entity();
        let entity_id = entity.entity_id();
        let rx = cx.prompt_for_new_path(&start, Some(&name));
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let _ = cx.with_window(entity_id, move |_window, cx| {
                entity.update(cx, move |this, cx| {
                    // 记住这次选的目录：连着下载多个文件不必反复挑位置。
                    if let Some(parent) = path.parent() {
                        let parent = parent.to_path_buf();
                        if this.download_dir.as_deref() != Some(parent.as_path()) {
                            this.download_dir = Some(parent);
                            this.persist_local_files(cx);
                        }
                    }
                    this.start_download(tab_id, path, remote, false, cx);
                });
            });
        })
        .detach();
    }

    /// 行菜单「下载到本机面板」：直接落到「本机」页当前目录，省掉保存对话框。
    ///
    /// 落点就是本机页顶部那条路径，看得见才敢写；本机页还没目录时菜单项是灰的。
    pub(super) fn download_to_local_pane(
        &mut self,
        tab_id: usize,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(local_dir) = self.local.path.clone() else {
            return;
        };
        let Some(remote_dir) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.files.path.clone())
        else {
            return;
        };
        let Some(safe) = safe_local_name(&name) else {
            self.file_error(tab_id, "文件名不合法，无法在本机保存", cx);
            return;
        };
        let remote = crate::remote_fs::child(&remote_dir, &name);
        // 落点已有同名文件时自动加序号，而不是无声覆盖——这条路没有保存对话框，
        // 用户看不到"将要覆盖"这件事。
        let target = unique_local_path(local_dir.join(&safe));
        self.start_download(tab_id, target, remote, true, cx);
    }

    /// 单个文件下载：进度写在面板底栏，完成后提示结果。
    ///
    /// `refresh_local_pane` 给「下载到本机面板」用：文件落盘后本机页要能立刻看见它。
    pub(super) fn start_download(
        &mut self,
        tab_id: usize,
        local: PathBuf,
        remote: String,
        refresh_local_pane: bool,
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
        // 「下载」不再弹对话框，落点只能靠完成提示交代——把保存路径写进去。
        let landed = local.display().to_string();
        self.status = format!("正在下载 {name}…").into();
        self.flash_transfer(format!("正在下载 {name}…"), IconName::ArrowDown, false, cx);
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
                    this.status = message.clone().into();
                    this.flash_transfer(message, IconName::TriangleAlert, true, cx);
                    cx.notify();
                });
                return;
            }
            let mut last_push: Option<std::time::Instant> = None;
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
                        // 进度消息节流到 100ms 一条（理由同上传路径）。
                        let now = std::time::Instant::now();
                        if last_push.is_some_and(|last| now.duration_since(last) < PROGRESS_UI_INTERVAL) {
                            continue;
                        }
                        last_push = Some(now);
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
                        "下载完成 · {} · {:.1} 秒 · 保存到 {landed}",
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
            let downloaded = error.is_none();
            let _ = this.update(cx, |this, cx| {
                if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                    tab.files.busy = None;
                    tab.files.progress = None;
                    tab.files.error = error.map(Into::into);
                }
                this.status = message.clone().into();
                this.flash_transfer(
                    message,
                    if downloaded {
                        IconName::CircleCheck
                    } else {
                        IconName::TriangleAlert
                    },
                    true,
                    cx,
                );
                if refresh_local_pane && downloaded {
                    this.local_refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 拖文件到终端：逐个用 lrzsz（`rz`）上传到 shell 当前目录。
    ///
    /// 与面板上传不同，落点由远端 shell 决定，这里不拼任何路径；
    /// 每个文件一个 `rz` 会话，失败后继续传下一个，最后汇总。
    pub(super) fn upload_via_terminal(
        &mut self,
        tab_id: usize,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.zmodem.is_some() {
            self.status = tr!(cx, "已有 lrzsz 上传进行中，请稍候");
            cx.notify();
            return;
        }
        if tab.ended {
            self.status = tr!(cx, "会话已断开，无法上传文件");
            cx.notify();
            return;
        }
        let Some(sender) = tab.commands.clone() else {
            self.status = tr!(cx, "本地终端不支持 lrzsz 上传，请使用 SSH 会话");
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
            let skipped = skipped.join("；");
            self.status = tr!(cx, "没有可上传的文件：{skipped}", skipped = skipped);
            cx.notify();
            return;
        }
        let total = files.len();
        let note = if skipped.is_empty() {
            String::new()
        } else {
            format!("，跳过 {} 项", skipped.len())
        };
        // 批次级取消标记：UI 的「取消」置位它，剩余文件不再发起；
        // 单个传输的取消经 SessionCommand::ZmodemCancel 下发到会话。
        let batch_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notice = tr!(
            cx,
            "正在通过 lrzsz 上传 {total} 个文件…{note}",
            total = total,
            note = note
        );
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.zmodem = Some(notice.clone());
            tab.zmodem_progress = Some((0, 0));
            tab.zmodem_cancel = Some(batch_cancel.clone());
        }
        self.status = notice.clone();
        self.flash_transfer(notice, IconName::ArrowUp, false, cx);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let mut failed = Vec::new();
            let mut done_count = 0usize;
            for path in files {
                // 批次取消：剩余文件不再发起，已传完的保留在远端。
                if batch_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
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
                        tab.zmodem = Some(tr!(cx, "正在通过 lrzsz 上传 {name}…", name = name));
                        cx.notify();
                    }
                });
                let mut last_push: Option<std::time::Instant> = None;
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
                            // 进度消息节流到 100ms 一条（理由同上传路径）。
                            let now = std::time::Instant::now();
                            if last_push.is_some_and(|last| now.duration_since(last) < PROGRESS_UI_INTERVAL) {
                                continue;
                            }
                            last_push = Some(now);
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
                } else {
                    done_count += 1;
                }
            }
            let canceled = batch_cancel.load(std::sync::atomic::Ordering::Relaxed);
            let failed_count = failed.len();
            let failed_list = failed.join("；");
            let done = done_count.to_string();
            let total_count = total.to_string();
            let _ = this.update(cx, |this, cx| {
                if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                    tab.zmodem = None;
                    tab.zmodem_progress = None;
                    tab.zmodem_cancel = None;
                }
                let message = if canceled {
                    tr!(
                        cx,
                        "lrzsz 上传已取消（成功 {done} / {total} 个文件）",
                        done = done,
                        total = total_count
                    )
                } else if failed_count == 0 {
                    tr!(cx, "lrzsz 上传完成 · {total} 个文件", total = total_count)
                } else {
                    tr!(
                        cx,
                        "lrzsz 上传结束 · {failed} 失败：{list}",
                        failed = failed_count,
                        list = failed_list
                    )
                };
                this.status = message.clone();
                this.flash_transfer(
                    message,
                    if failed_count == 0 && !canceled {
                        IconName::CircleCheck
                    } else {
                        IconName::TriangleAlert
                    },
                    true,
                    cx,
                );
                cx.notify();
            });
        })
        .detach();
    }

    /// 取消本轮 lrzsz 上传：置位批次取消标记（剩余文件不再发起），并给
    /// 会话下发 `ZmodemCancel` 让当前传输尽快收场（摘掉下行接管点恢复
    /// 终端输出、往远端发 Ctrl+C）。
    pub(super) fn cancel_zmodem(&mut self, tab_id: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        if tab.zmodem.is_none() {
            return;
        }
        if let Some(flag) = tab.zmodem_cancel.as_ref() {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(sender) = tab.commands.as_ref() {
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let _ = sender.send(crate::session::SessionCommand::ZmodemCancel { reply });
        }
        tab.zmodem = Some(tr!(cx, "正在取消 lrzsz 上传…"));
        self.status = tr!(cx, "正在取消 lrzsz 上传…");
        cx.notify();
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
                    let message =
                        format!("{action}失败：{}", crate::session::error_summary(&error));
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
    /// `target` 决定这份排序 / 列宽记在谁身上（远端标签或本机面板）。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn file_sort_header(
        &self,
        id: &'static str,
        label: SharedString,
        key: FileSortKey,
        width: Option<(usize, f32)>,
        right_align: bool,
        target: Option<PaneTarget>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let state = target.and_then(|pane| self.pane_sort(pane));
        let active = state.is_some_and(|(sort, _)| sort == key);
        let desc = state.is_some_and(|(sort, d)| sort == key && d);

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
        let cell = match target {
            Some(pane) => cell
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_pane_sort(pane, key, cx);
                })),
            // 没有可操作的面板（会话断了 / 还没连上）：表头照常显示，点了没反应。
            None => cell.child(text),
        };
        let Some((col, width)) = width else {
            return cell.into_any_element();
        };
        let Some(pane) = target else {
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
                        target: pane,
                        col,
                        start_x: event.position.x.as_f32(),
                        start_w: width,
                    });
                }),
            )
            .on_drag(ColumnDrag { target: pane, col }, |_, _, _, cx| {
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

    /// 某面板当前的排序状态；面板还没打开过（远端标签已不存在）时为 None。
    pub(super) fn pane_sort(&self, target: PaneTarget) -> Option<(FileSortKey, bool)> {
        match target {
            PaneTarget::Remote(tab_id) => self
                .tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .map(|tab| (tab.files.sort, tab.files.sort_desc)),
            PaneTarget::Local => Some((self.local.sort, self.local.sort_desc)),
        }
    }

    /// 某面板当前的目录字符串（路径栏与弹窗都用它）；还没打开过时为空串。
    pub(super) fn pane_path(&self, target: PaneTarget) -> String {
        match target {
            PaneTarget::Remote(tab_id) => self
                .tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .and_then(|tab| tab.files.path.clone())
                .unwrap_or_default(),
            PaneTarget::Local => self
                .local
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    /// 点列名：同一列翻转升降序，换列则从升序开始；排完立刻重绘。
    pub(super) fn set_pane_sort(
        &mut self,
        target: PaneTarget,
        key: FileSortKey,
        cx: &mut Context<Self>,
    ) {
        match target {
            PaneTarget::Remote(tab_id) => {
                let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                    return;
                };
                if tab.files.sort == key {
                    tab.files.sort_desc = !tab.files.sort_desc;
                } else {
                    tab.files.sort = key;
                    tab.files.sort_desc = false;
                }
                tab.files.sort_entries();
            }
            PaneTarget::Local => {
                if self.local.sort == key {
                    self.local.sort_desc = !self.local.sort_desc;
                } else {
                    self.local.sort = key;
                    self.local.sort_desc = false;
                }
                self.local.sort_entries();
            }
        }
        cx.notify();
    }

    /// 列宽拖拽落到哪张表上：远端各标签独立记忆，本机一份全局记忆。返回是否真的改了宽度。
    pub(super) fn set_pane_col(&mut self, target: PaneTarget, col: usize, width: f32) -> bool {
        let cols = match target {
            PaneTarget::Remote(tab_id) => match self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                Some(tab) => &mut tab.files.cols,
                None => return false,
            },
            PaneTarget::Local => &mut self.local.cols,
        };
        if (cols[col] - width).abs() <= 0.5 {
            return false;
        }
        cols[col] = width;
        true
    }

    /// 窄栏判定：边栏面板实测宽度不够摆五列时，整列隐掉时间和权限，
    /// 名称列才有地方喘气（列名与单元格都得跟着少两格）。
    pub(super) fn pane_narrow(&self, cx: &App) -> bool {
        let width = self.split_width.get() * self.panel_ratio(cx);
        width > 0.0 && width < FILE_PANE_NARROW_W
    }

    /// 一行的可视部分：图标 + 名称 + 类型 / 大小 /（宽栏再加）修改时间 / 权限。
    ///
    /// 返回子元素列表而不是整行，行本身的 id、悬停、拖放与右键菜单由各自的
    /// 面板决定——远端行能上传、本机行不能。
    pub(super) fn file_row_children(
        entry: &crate::remote_fs::Entry,
        cols: [f32; 4],
        narrow: bool,
        pal: Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
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
        let mut cells = vec![
            Icon::new(if is_dir {
                IconName::FolderOpen
            } else {
                IconName::File
            })
            .small()
            .flex_shrink_0()
            .text_color(if is_dir { pal.link } else { pal.muted })
            .into_any_element(),
            div()
                .flex_1()
                .min_w(px(48.0))
                .text_ellipsis()
                .text_sm()
                .child(entry.name.clone())
                .into_any_element(),
            div()
                .w(px(cols[0]))
                .flex_shrink_0()
                .text_xs()
                .text_color(pal.muted)
                .child(kind_text)
                .into_any_element(),
            div()
                .w(px(cols[1]))
                .flex_shrink_0()
                .text_xs()
                .text_right()
                .text_color(pal.muted)
                .child(size_text)
                .into_any_element(),
        ];
        if !narrow {
            cells.extend([
                div()
                    .w(px(cols[2]))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(mtime)
                    .into_any_element(),
                div()
                    .w(px(cols[3]))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(entry.permissions_text())
                    .into_any_element(),
            ]);
        }
        cells
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

        let up_enabled = path
            .as_deref()
            .and_then(crate::remote_fs::parent_of)
            .is_some();
        // 表格状态记在面板上，路径栏编辑框也按面板认领：本机页与远端页互不打架。
        let pane = live.map(PaneTarget::Remote);
        // 「本机」页有没有目录，决定远端行的「下载到本机面板」能不能用。
        let local_pane_ready = self.local.path.is_some();
        let narrow = self.pane_narrow(cx);
        let editing = self
            .path_edit
            .as_ref()
            .filter(|(target, _)| Some(*target) == pane)
            .map(|(_, input)| input.clone());
        let mut path_row = h_flex()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .py_1()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(pal.border);
        if let Some(input) = editing {
            path_row = path_row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(styled_input(Input::new(&input).text_xs(), &pal)),
            );
        } else {
            let pane_for_edit = pane;
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
                        if let Some(pane) = pane_for_edit {
                            this.start_path_edit(pane, window, cx);
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
                    let cells = Self::file_row_children(&entry, cols, narrow, pal, cx);
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
                    let row = row.children(cells);
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
                    let download_label = tr!(cx, "下载");
                    let download_pick_label = tr!(cx, "下载到…（换目录）");
                    let to_local_label = if local_pane_ready {
                        tr!(cx, "下载到本机面板")
                    } else {
                        tr!(cx, "下载到本机面板（先在「本机」页打开一个目录）")
                    };
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
                            let menu =
                                menu.item(PopupMenuItem::new(download_label.clone()).on_click(
                                    move |_, _, cx| {
                                        downloading.update(cx, |this, cx| {
                                            this.download_file(id, download_name.clone(), cx);
                                        });
                                    },
                                ));
                            let picking = menu_entity.clone();
                            let pick_name = name.clone();
                            let menu = menu.item(
                                PopupMenuItem::new(download_pick_label.clone()).on_click(
                                    move |_, _, cx| {
                                        picking.update(cx, |this, cx| {
                                            this.prompt_download_file(id, pick_name.clone(), cx);
                                        });
                                    },
                                ),
                            );
                            let to_local = menu_entity.clone();
                            let to_local_name = name.clone();
                            menu.item(
                                PopupMenuItem::new(to_local_label.clone())
                                    .disabled(!local_pane_ready)
                                    .on_click(move |_, _, cx| {
                                        to_local.update(cx, |this, cx| {
                                            this.download_to_local_pane(
                                                id,
                                                to_local_name.clone(),
                                                cx,
                                            );
                                        });
                                    }),
                            )
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
        let list = div()
            .id("file-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar();
        // 没有活的 SFTP 会话就不接拖放：落点提示得有目标目录才有意义。
        let list = if live.is_some() {
            list.drag_over::<ExternalPaths>(move |style, paths, _, cx| {
                hint_entity.update(cx, |this, _| {
                    this.drop_hint
                        .set(Some((hint_dir.clone().into(), paths.paths().len())));
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
                pane,
                cx,
            ))
            .child(self.file_sort_header(
                "files-sort-kind",
                head_labels.1.clone(),
                FileSortKey::Kind,
                Some((0, cols[0])),
                false,
                pane,
                cx,
            ))
            .child(self.file_sort_header(
                "files-sort-size",
                head_labels.2.clone(),
                FileSortKey::Size,
                Some((1, cols[1])),
                true,
                pane,
                cx,
            ))
            // 窄栏时时间与权限整列隐掉（行内同样少这两格），名称列才有地方喘气。
            .when(!narrow, |this| {
                this.child(self.file_sort_header(
                    "files-sort-time",
                    head_labels.3.clone(),
                    FileSortKey::Modified,
                    Some((2, cols[2])),
                    false,
                    pane,
                    cx,
                ))
                .child(self.file_sort_header(
                    "files-sort-perm",
                    head_labels.4.clone(),
                    FileSortKey::Permissions,
                    Some((3, cols[3])),
                    false,
                    pane,
                    cx,
                ))
            })
            // 拖拽列宽：监听整行，指针离开手柄也不中断；释放时清掉起点状态。
            .on_drag_move::<ColumnDrag>(cx.listener(
                |this, event: &DragMoveEvent<ColumnDrag>, _, cx| {
                    let drag = *event.drag(cx);
                    let Some(start) = this.col_drag else {
                        return;
                    };
                    if start.target != drag.target || start.col != drag.col {
                        return;
                    }
                    let width = (start.start_w + (event.event.position.x.as_f32() - start.start_x))
                        .clamp(FILE_COL_MIN_W, FILE_COL_MAX_W);
                    if this.set_pane_col(drag.target, drag.col, width) {
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
                    // 定位目标：终端上报的目录（cd 猜测 / OSC 7）；没有上报时不猜，
                    // 明确回落到登录主目录，并在提示里说清当前识别到了哪个目录。
                    let detected = tab.and_then(|tab| tab.terminal_cwd.clone());
                    let target_path = detected.clone();
                    let locate_tooltip = match detected.as_deref() {
                        Some(path) => tr!(cx, "定位到终端当前目录：{path}", path = path),
                        None => tr!(cx, "终端还没上报当前目录，点击跳到登录主目录"),
                    };
                    let follow_tooltip = match (follow_on, detected.as_deref()) {
                        (true, Some(path)) => tr!(cx, "跟随终端：开，当前 {path}", path = path),
                        (true, None) => tr!(cx, "跟随终端：开（等待终端上报目录）"),
                        (false, _) => tr!(cx, "跟随终端目录"),
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
                                .tooltip(locate_tooltip)
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
                                        // 开启跟随时立刻对齐到终端目录；还没上报就不跳
                                        // （跳到主目录会被当成「跟到了」，反而看不出没生效）。
                                        if turn_on && let Some(target) = target.as_deref() {
                                            let current = this
                                                .tabs
                                                .iter()
                                                .find(|t| t.id == id)
                                                .and_then(|t| t.files.path.clone());
                                            if current.as_deref() != Some(target) {
                                                this.load_files(id, Some(target.to_string()), cx);
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

    /// 本地副本改动后的回传确认（对照 WindTerm 的 Upload File 弹窗）。
    pub(super) fn render_file_upload_prompt(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(prompt) = self.file_upload_prompt.as_ref() else {
            return div().into_any_element();
        };
        let title = tr!(cx, "远程文件已被修改");
        let message = tr!(cx, "是否要将此文件上传回服务器？");
        let name_label = tr!(cx, "文件名称");
        let server_label = tr!(cx, "服务器");
        let path_label = tr!(cx, "远程路径");
        let yes_label = tr!(cx, "是");
        let yes_all_label = tr!(cx, "全部是");
        let no_label = tr!(cx, "否");
        let name = prompt.name.clone();
        let server = prompt.server.clone();
        let remote = prompt.remote.clone();
        let field = |label: SharedString, value: SharedString| {
            h_flex()
                .w_full()
                .gap_3()
                .child(
                    div()
                        .w(px(72.0))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_ellipsis()
                        .child(value),
                )
        };
        v_flex()
            .id("file-upload-backdrop")
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
                    .id("file-upload-dialog")
                    .w(px(420.0))
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
                    .child(div().text_sm().child(message))
                    .child(field(name_label, name.into()))
                    .child(field(server_label, server))
                    .child(field(path_label, remote.into()))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("file-upload-yes")
                                    .label(yes_label)
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.answer_remote_upload(true, false, cx);
                                    })),
                            )
                            .child(
                                Button::new("file-upload-yes-all")
                                    .label(yes_all_label)
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.answer_remote_upload(true, true, cx);
                                    })),
                            )
                            .child(
                                Button::new("file-upload-no")
                                    .label(no_label)
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.answer_remote_upload(false, false, cx);
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
mod browser_tests {
    use super::{
        FileBrowser, FileSortKey, FileStamp, OpenedRemote, file_stamp, format_remote_time_short,
        poll_opened_files, safe_local_name,
    };
    use crate::remote_fs;
    use std::path::PathBuf;

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
        let mut files = FileBrowser {
            entries: vec![
                entry("zeta", true, 0, None),
                entry("Beta.txt", false, 20, Some(2)),
                entry("alpha.txt", false, 10, Some(3)),
            ],
            ..FileBrowser::default()
        };
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

    fn stamp(len: u64, secs: u64) -> FileStamp {
        FileStamp {
            len,
            modified: Some((secs, 0)),
        }
    }

    fn opened() -> OpenedRemote {
        OpenedRemote::new(
            PathBuf::from("unused"),
            "/tmp/a.txt".to_string(),
            "a.txt".to_string(),
            stamp(3, 10),
        )
    }

    #[test]
    fn opened_remote_waits_for_a_stable_stamp_before_uploading() {
        let mut item = opened();
        // 第一次看到新标记：编辑器可能还在写，先记候选。
        assert!(!item.ready_to_upload(stamp(5, 11)));
        // 连续两轮同一个标记：放行并排队。
        assert!(item.ready_to_upload(stamp(5, 11)));
        item.pending = Some(stamp(5, 11));
        assert!(item.begin_upload(stamp(5, 11), Some(stamp(5, 11))));
        assert!(item.finish_upload(stamp(5, 11), true));
        // 已经回传过的版本不再重复上传。
        assert!(!item.ready_to_upload(stamp(5, 11)));
        // 再写一次要重新走两轮确认。
        assert!(!item.ready_to_upload(stamp(7, 12)));
        assert!(item.ready_to_upload(stamp(7, 12)));
    }

    #[test]
    fn file_stamp_changes_with_content_and_reports_missing_files() {
        let path = std::env::temp_dir().join(format!(
            "ruterm-stamp-{}-{}.txt",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, b"one").unwrap();
        let first = file_stamp(&path).expect("刚写入的文件应有标记");
        std::fs::write(&path, b"one-two").unwrap();
        let second = file_stamp(&path).expect("追加后仍应有标记");
        assert_ne!(first, second);
        std::fs::remove_file(&path).unwrap();
        assert!(file_stamp(&path).is_none());
    }

    #[test]
    fn poll_opened_files_uploads_once_after_a_stable_change() {
        let dir =
            std::env::temp_dir().join(format!("ruterm-poll-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("a.txt");
        std::fs::write(&local, b"one").unwrap();

        let mut browser = FileBrowser::default();
        browser.opened.push(OpenedRemote::new(
            local.clone(),
            "/tmp/a.txt".to_string(),
            "a.txt".to_string(),
            file_stamp(&local).unwrap(),
        ));

        // 没动过：不回传。
        assert!(poll_opened_files(&mut browser).is_none());
        // 刚写完一轮：可能还没写完，先等下一轮。
        std::fs::write(&local, b"one-two").unwrap();
        assert!(poll_opened_files(&mut browser).is_none());
        // 同一个标记再次出现：回传，并且只回传一次。
        let (path, remote, name) = poll_opened_files(&mut browser).expect("稳定修改应触发回传");
        assert_eq!(path, local);
        assert_eq!(remote, "/tmp/a.txt");
        assert_eq!(name, "a.txt");
        assert!(poll_opened_files(&mut browser).is_none());
        // 本地副本被删：跟踪项清理掉。
        std::fs::remove_file(&local).unwrap();
        assert!(poll_opened_files(&mut browser).is_none());
        assert!(browser.opened.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
