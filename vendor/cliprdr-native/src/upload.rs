//! 上传进度登记：界面写好剪贴板前登记这批文件，剪贴板线程办理远端读取时累加字节。
//!
//! `WinClipboard` 由 ironrdp-client 自己创建，界面拿不到句柄，所以用进程级登记表对接：
//! 剪贴板本来也只有一份，一次只登记一批；剪贴板线程发现同一批路径时认领进度槽位，
//! 之后每次应答 `RANGE` 请求都记一笔，界面按固定节奏取快照画进度条。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 一次上传的进度快照来源。
#[derive(Debug, Default)]
pub struct UploadProgress {
    total: AtomicU64,
    sent: AtomicU64,
    files: AtomicU32,
    acknowledged: AtomicBool,
    started: AtomicBool,
    failed: AtomicBool,
}

impl UploadProgress {
    /// 这批文件的总字节数（目录不计）。
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// 已经发给远端的字节数。
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// 文件个数（目录不计）。
    pub fn files(&self) -> u32 {
        self.files.load(Ordering::Relaxed)
    }

    /// 剪贴板线程已经见过这批文件，并把它报给了远端。
    pub fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// 远端收下了文件列表（Format List Response），可以替用户按 Ctrl+V 了。
    pub fn acknowledged(&self) -> bool {
        self.acknowledged.load(Ordering::Relaxed)
    }

    /// 传输失败（本机读文件出错、远端请求越界，或界面判定远端已经不再读）。
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// 传完了：全部字节都已发出，或者已经失败。
    pub fn finished(&self) -> bool {
        self.failed() || (self.acknowledged() && self.sent() >= self.total())
    }

    /// 界面侧判定这次上传不再有进展（远端没开始读、或读到一半停了）。
    pub fn abort(&self) {
        self.failed.store(true, Ordering::Relaxed);
    }

    pub(crate) fn start(&self, total: u64, files: u32) {
        self.total.store(total, Ordering::Relaxed);
        self.files.store(files, Ordering::Relaxed);
        self.started.store(true, Ordering::Relaxed);
    }

    pub(crate) fn mark_acknowledged(&self) {
        self.acknowledged.store(true, Ordering::Relaxed);
    }

    /// 记一次成功应答的字节数。远端重读同一段时不重复累加，也不越过总大小。
    pub(crate) fn add_sent(&self, bytes: usize) {
        if self.failed() {
            return;
        }
        // 远端开始读实际字节，说明文件列表已经收下了；有的服务端只发请求、不发
        // Format List Response，不能只靠回执判断。
        self.acknowledged.store(true, Ordering::Relaxed);
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let sent = self.sent().saturating_add(bytes);
        let total = self.total();
        let sent = if total == 0 { sent } else { sent.min(total) };
        self.sent.store(sent, Ordering::Relaxed);
    }

    pub(crate) fn mark_failed(&self) {
        self.failed.store(true, Ordering::Relaxed);
    }
}

/// 界面登记的待传文件，等剪贴板线程按路径认领。
struct Pending {
    roots: Vec<PathBuf>,
    progress: Arc<UploadProgress>,
    attached: bool,
}

static PENDING: OnceLock<Mutex<Option<Pending>>> = OnceLock::new();

fn pending() -> &'static Mutex<Option<Pending>> {
    PENDING.get_or_init(|| Mutex::new(None))
}

/// 登记一批要传给远端的文件，返回供界面轮询的进度槽位。
///
/// 在写系统剪贴板之前调用：剪贴板线程随后枚举到同一批路径时会把进度挂上去。
pub fn begin_upload(paths: &[PathBuf]) -> Arc<UploadProgress> {
    let progress = Arc::new(UploadProgress::default());
    if let Ok(mut slot) = pending().lock() {
        *slot = Some(Pending {
            roots: paths.to_vec(),
            progress: Arc::clone(&progress),
            attached: false,
        });
    }
    progress
}

/// 撤销登记（剪贴板没写成功，或标签已经关掉）。
pub fn cancel_upload(progress: &Arc<UploadProgress>) {
    let Ok(mut slot) = pending().lock() else {
        return;
    };
    if slot
        .as_ref()
        .is_some_and(|pending| Arc::ptr_eq(&pending.progress, progress))
    {
        *slot = None;
    }
}

/// 剪贴板线程枚举出本地文件后调用：路径对得上就认领进度槽位并记下总大小。
pub(crate) fn attach(paths: &[&Path], total: u64, files: u32) -> Option<Arc<UploadProgress>> {
    let mut slot = pending().lock().ok()?;
    let pending = slot.as_mut()?;
    if pending.attached {
        return None;
    }
    let matches = paths
        .iter()
        .any(|path| pending.roots.iter().any(|root| root == path));
    if !matches {
        return None;
    }
    pending.attached = true;
    pending.progress.start(total, files);
    Some(Arc::clone(&pending.progress))
}

/// 有一批文件正在传给远端。
///
/// 这段时间里剪贴板会短暂变空（或刚被远端的文件对象接管），拿空列表报给远端会把
/// 远端正读着的文件剪贴板清掉，正在进行的粘贴就会半途而废。
pub(crate) fn in_flight() -> bool {
    let Ok(slot) = pending().lock() else {
        return false;
    };
    slot.as_ref()
        .is_some_and(|pending| pending.attached && !pending.progress.finished())
}
