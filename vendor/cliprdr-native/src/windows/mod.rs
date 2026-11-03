mod clipboard_data_ref;
mod clipboard_impl;
mod cliprdr_backend;
mod data_object;
pub(crate) mod file_list;
mod local_files;
mod os_clipboard;
mod remote_format_registry;
mod shared;
mod transport;
mod utils;

use std::rc::Rc;
use std::sync::{Arc, mpsc, mpsc as mpsc_sync};

use ironrdp_cliprdr::backend::{ClipboardMessageProxy, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileDescriptor,
    FormatDataRequest, FormatDataResponse, LockDataId,
};
use tracing::{debug, warn};
pub use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::{CLIPBRD_E_CANT_OPEN, E_ACCESSDENIED, FALSE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::AddClipboardFormatListener;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::SetWindowSubclass;
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExA, DefWindowProcA, DestroyWindow, DispatchMessageW, GetClassInfoA, GetMessageW,
    MSG, PostThreadMessageW, RegisterClassA, TranslateMessage, WINDOW_EX_STYLE, WM_QUIT, WM_USER, WNDCLASSA, WS_POPUP,
};
use windows::core::{Error, s};

use self::clipboard_impl::{ClipboardWindowState, WinClipboardImpl, clipboard_subproc, release};
use self::cliprdr_backend::WinCliprdrBackend;
use self::shared::ClipboardShared;

const BACKEND_CHANNEL_SIZE: usize = 8;
const WM_CLIPRDR_BACKEND_EVENT: u32 = WM_USER;

pub type WinCliprdrResult<T> = Result<T, WinCliprdrError>;

#[derive(Debug)]
pub enum WinCliprdrError {
    AddClipboardFormatListener,
    FormatsEnumeration,
    ClipboardAccessDenied,
    ClipboardOpen,
    ClipboardEmpty,
    Utf16Conversion,
    ClipboardData,
    SetClipboardData,
    WindowSubclass,
    Alloc,
    DataReceiveTimeout,
    RenderFormat,
    ThreadSpawn,
    WinAPI(Error),
}

impl core::fmt::Display for WinCliprdrError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WinCliprdrError::AddClipboardFormatListener => write!(f, "failed to register clipboard format listener"),
            WinCliprdrError::FormatsEnumeration => {
                write!(f, "failed to enumerate formats available in the current clipboard")
            }
            WinCliprdrError::ClipboardAccessDenied => write!(f, "clipboard is busy or denied"),
            WinCliprdrError::ClipboardOpen => write!(f, "failed to open the clipboard"),
            WinCliprdrError::ClipboardEmpty => write!(f, "failed to empty the clipboard"),
            WinCliprdrError::Utf16Conversion => write!(f, "failed to convert UTF-16 string to UTF-8"),
            WinCliprdrError::ClipboardData => write!(f, "failed to get current clipboard data"),
            WinCliprdrError::SetClipboardData => write!(f, "failed to set clipboard data"),
            WinCliprdrError::WindowSubclass => write!(f, "failed to subclass window"),
            WinCliprdrError::Alloc => write!(f, "failed to allocate global memory"),
            WinCliprdrError::DataReceiveTimeout => write!(f, "failed to receive data from remote clipboard"),
            WinCliprdrError::RenderFormat => write!(f, "failed to render clipboard format"),
            WinCliprdrError::ThreadSpawn => write!(f, "failed to spawn the clipboard thread"),
            WinCliprdrError::WinAPI(_error) => write!(f, "WinAPI error"),
        }
    }
}

impl core::error::Error for WinCliprdrError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            WinCliprdrError::AddClipboardFormatListener => None,
            WinCliprdrError::FormatsEnumeration => None,
            WinCliprdrError::ClipboardAccessDenied => None,
            WinCliprdrError::ClipboardOpen => None,
            WinCliprdrError::ClipboardEmpty => None,
            WinCliprdrError::Utf16Conversion => None,
            WinCliprdrError::ClipboardData => None,
            WinCliprdrError::SetClipboardData => None,
            WinCliprdrError::WindowSubclass => None,
            WinCliprdrError::Alloc => None,
            WinCliprdrError::DataReceiveTimeout => None,
            WinCliprdrError::RenderFormat => None,
            WinCliprdrError::ThreadSpawn => None,
            WinCliprdrError::WinAPI(error) => Some(error),
        }
    }
}

impl From<Error> for WinCliprdrError {
    fn from(err: Error) -> Self {
        // `CLIPBRD_E_CANT_OPEN` is what OLE reports when another application holds the clipboard.
        if err.code() == E_ACCESSDENIED || err.code() == CLIPBRD_E_CANT_OPEN {
            WinCliprdrError::ClipboardAccessDenied
        } else {
            WinCliprdrError::WinAPI(err)
        }
    }
}

/// Sent from the clipboard backend shim to the actual WinAPI subproc event loop
#[derive(Debug)]
pub(crate) enum BackendEvent {
    // Events generated by OS event loop
    ClipboardUpdated,
    RenderFormat(ClipboardFormatId),
    RenderAllFormats,

    // PDU processing events
    /// A backend was built for a new `CLIPRDR` channel.
    SessionStarted,
    DowngradedCapabilities(ClipboardGeneralCapabilityFlags),
    Ready,
    /// The remote format list, with the generation it starts.
    RemoteFormatList(Vec<ClipboardFormat>, u64),
    FormatDataRequest(FormatDataRequest),
    FormatDataResponse(FormatDataResponse<'static>),
    RemoteRequestsFormatList,
    /// The remote answered a format list this side sent: `true` if it accepted it.
    FormatListResponse(bool),
    RemoteFileList {
        files: Vec<FileDescriptor>,
        clip_data_id: Option<u32>,
    },
    FileContentsRequest(FileContentsRequest),
    Lock(LockDataId),
    Unlock(LockDataId),
}

/// Windows RDP client clipboard implementation.
///
/// IronRDP client implementation should provide raw window handle and message proxy to send
/// messages from the backend to `CLIPRDR` SVC.
///
/// [`WinClipboard`] instance holds ownership of the actual clipboard backend processing logic
/// and should be kept alive during the whole lifetime of the application.
///
/// # Threading
///
/// The hidden window, its subclass and OLE live on a dedicated thread owned by this type,
/// which runs its own blocking message loop. This is required: `WM_RENDERFORMAT` must fetch
/// the remote data before returning, while the answer comes from the `CLIPRDR` session loop;
/// on a shared thread the two would wait on each other until the fetch times out. The embedder
/// no longer has to pump this window's messages.
///
/// # File transfer
///
/// Files are copied in both directions ([MS-RDPECLIP] file stream protocol): local files
/// (`CF_HDROP`, or `FileGroupDescriptorW` from another application) are offered to the
/// remote, and remote files are put on the local clipboard as an OLE data object, so that
/// pasting into Explorer works. The latter needs a single-threaded apartment: the clipboard
/// thread is initialized as one whenever file transfer is enabled.
///
/// File transfer is off unless enabled with [`WinClipboard::with_file_transfer`]. The embedder
/// must then route [`ClipboardMessage::SendInitiateFileCopy`],
/// [`ClipboardMessage::SendFileContentsRequest`] and
/// [`ClipboardMessage::SendFileContentsResponse`] to `Cliprdr`, and drive
/// `Cliprdr::drive_timeouts`.
///
/// Remote files are read without the clipboard thread: their streams live in the process's
/// multithreaded apartment, so the pasting application's reads run on COM worker threads, with
/// several chunks requested ahead. Only fetching the file list waits on the clipboard thread,
/// once per paste. This also initializes the multithreaded apartment for the clipboard's
/// lifetime.
///
/// [MS-RDPECLIP]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeclip/fb9b7e0b-6db4-41c2-b83c-f889c1ee7688
/// [`ClipboardMessage::SendInitiateFileCopy`]: ironrdp_cliprdr::backend::ClipboardMessage::SendInitiateFileCopy
/// [`ClipboardMessage::SendFileContentsRequest`]: ironrdp_cliprdr::backend::ClipboardMessage::SendFileContentsRequest
/// [`ClipboardMessage::SendFileContentsResponse`]: ironrdp_cliprdr::backend::ClipboardMessage::SendFileContentsResponse
pub struct WinClipboard {
    window: HWND,
    backend_tx: mpsc_sync::SyncSender<BackendEvent>,
    file_transfer: bool,
    shared: Arc<ClipboardShared>,
    /// 剪贴板窗口所在线程：Drop 向它投递 `WM_QUIT`，由它自己收尾。
    thread_id: u32,
}

/// 剪贴板线程启动成功时交回父线程的东西：窗口句柄、事件通道与线程 id。
///
/// 句柄以整数传递：`HWND` 本身不是 `Send`，而窗口只属于工作线程，父线程只拿它投消息。
struct ClipboardThreadStart {
    window: isize,
    backend_tx: mpsc_sync::SyncSender<BackendEvent>,
    thread_id: u32,
}

impl WinClipboard {
    /// Creates new clipboard instance with file transfer enabled.
    ///
    /// Under the hood, a hidden window is created for capturing the clipboard events.
    pub fn new(message_proxy: impl ClipboardMessageProxy + 'static) -> WinCliprdrResult<Self> {
        // 本机副本：ironrdp-client 只会调用 `new`，这里默认开启文件剪贴板，
        // 文本与文件都走同一套 CLIPRDR 通道。
        Self::with_file_transfer(message_proxy, true)
    }

    /// Creates new clipboard instance, with file transfer enabled or not.
    ///
    /// With `file_transfer` on, the embedder must handle the file stream messages (see
    /// [`WinClipboard`]). With it off, the backend neither negotiates nor produces any, and file
    /// lists are not advertised in either direction.
    pub fn with_file_transfer(
        message_proxy: impl ClipboardMessageProxy + 'static,
        file_transfer: bool,
    ) -> WinCliprdrResult<Self> {
        let shared = Arc::new(ClipboardShared::new(Box::new(message_proxy)));
        let (start_tx, start_rx) = mpsc::channel();

        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ruterm-cliprdr".to_string())
            .spawn(move || clipboard_thread(thread_shared, file_transfer, start_tx))
            .map_err(|error| {
                debug!(%error, "Failed to spawn the clipboard thread");
                WinCliprdrError::ThreadSpawn
            })?;

        // 等线程把窗口建好再返回：之后它只负责收消息，不再回传结果。
        let start = start_rx.recv().map_err(|_| WinCliprdrError::ThreadSpawn)??;

        Ok(Self {
            window: HWND(start.window as *mut core::ffi::c_void),
            backend_tx: start.backend_tx,
            file_transfer,
            shared,
            thread_id: start.thread_id,
        })
    }

    /// Returns clipboard backend factory suitable for making backend instances for `CLIPRDR` SVC.
    pub fn backend_factory(&self) -> Box<dyn CliprdrBackendFactory + Send> {
        Box::new(WinCliprdrBackendFactory {
            tx: self.backend_tx.clone(),
            window: self.window,
            file_transfer: self.file_transfer,
            shared: Arc::clone(&self.shared),
        })
    }
}

impl Drop for WinClipboard {
    fn drop(&mut self) {
        // 窗口、子类与 OLE 都归工作线程；这里只叫它退出，不等它收尾，避免拖慢会话结束。
        // SAFETY: `PostThreadMessageW` 可以投递到其它线程的消息队列。
        if let Err(error) = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) } {
            warn!(%error, "Failed to stop the clipboard thread");
        }
    }
}

/// 剪贴板工作线程：建窗口、处理消息、结束时收尾。
///
/// `WM_RENDERFORMAT` 要同步等远端数据，而答复由 RDP 会话循环驱动；两者同线程会互相
/// 等死（直到超时），表现为界面卡顿。独立线程后，等待只阻塞这一个线程，会话照常取帧。
fn clipboard_thread(
    shared: Arc<ClipboardShared>,
    file_transfer: bool,
    started: mpsc::Sender<WinCliprdrResult<ClipboardThreadStart>>,
) {
    // 远端文件要作为 OLE 数据对象放进本机剪贴板：本线程只为剪贴板而生，直接初始化成 STA。
    let ole_initialized = if file_transfer {
        // SAFETY: 本线程新建，尚未初始化 COM；卸载在收尾时同线程完成。
        match unsafe { OleInitialize(None) } {
            Ok(()) => true,
            Err(error) => {
                debug!(%error, "Failed to initialize OLE; remote files can't be pasted");
                false
            }
        }
    } else {
        false
    };

    let (window, state, backend_tx) = match setup_window(Arc::clone(&shared), file_transfer, ole_initialized) {
        Ok(setup) => setup,
        Err(error) => {
            if ole_initialized {
                // SAFETY: 与上面的 `OleInitialize` 配对，同线程。
                unsafe { OleUninitialize() };
            }
            let _ = started.send(Err(error));
            return;
        }
    };

    let start = ClipboardThreadStart {
        window: window.0 as isize,
        backend_tx,
        // SAFETY: Always safe to call.
        thread_id: unsafe { GetCurrentThreadId() },
    };
    if started.send(Ok(start)).is_err() {
        // 父线程在启动完成前就没了：收拾干净再退出。
        release(window, state);
        // SAFETY: 窗口属于本线程。
        unsafe {
            let _ = DestroyWindow(window);
        }
        return;
    }

    let mut message = MSG::default();
    loop {
        // SAFETY: 标准的线程消息循环；`WM_QUIT` 让 `GetMessageW` 返回 0。
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        // SAFETY: 消息由 `GetMessageW` 取到，按常规分发。
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    // `release` 摘掉监听与子类、关掉传输并（必要时）卸载 OLE。
    release(window, state);
    // SAFETY: 窗口属于本线程。
    unsafe {
        let _ = DestroyWindow(window);
    }
}

/// 在本线程建隐藏窗口、挂剪贴板监听与子类，返回窗口、状态与事件通道。
fn setup_window(
    shared: Arc<ClipboardShared>,
    file_transfer: bool,
    ole_available: bool,
) -> WinCliprdrResult<(HWND, Rc<ClipboardWindowState>, mpsc_sync::SyncSender<BackendEvent>)> {
    extern "system" fn wndproc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // SAFETY: default handler
        unsafe { DefWindowProcA(window, message, wparam, lparam) }
    }

    // SAFETY: low-level WinAPI call
    let instance = unsafe { GetModuleHandleA(None)? };
    let window_class = s!("IronRDPClipboardMonitor");

    let mut existing_wc = WNDCLASSA::default();
    // SAFETY: `instance` is a valid module handle, `window_class` is a valid null-terminated string,
    // and `existing_wc` is a valid mutable reference to a WNDCLASSA structure.
    let class_exists = unsafe { GetClassInfoA(Some(instance.into()), window_class, &mut existing_wc).is_ok() };

    if !class_exists {
        let wc = WNDCLASSA {
            hInstance: instance.into(),
            lpszClassName: window_class,
            lpfnWndProc: Some(wndproc),
            ..Default::default()
        };

        // SAFETY: low-level WinAPI call
        let atom = unsafe { RegisterClassA(&wc) };
        if atom == 0 {
            return Err(WinCliprdrError::from(Error::from_thread()));
        }
    }

    // SAFETY: low-level WinAPI call
    let window = unsafe {
        CreateWindowExA(
            WINDOW_EX_STYLE::default(),
            window_class,
            None,
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };

    if window.is_invalid() {
        return Err(WinCliprdrError::from(Error::from_thread()));
    }
    // Init clipboard processing for WinAPI event loop
    //
    // SAFETY: `window` is a valid window handle
    unsafe { AddClipboardFormatListener(window)? };

    let (backend_tx, backend_rx) = mpsc_sync::sync_channel(BACKEND_CHANNEL_SIZE);

    let state = Rc::new(ClipboardWindowState::new(WinClipboardImpl::new(
        window,
        Arc::clone(&shared),
        backend_rx,
        file_transfer,
        ole_available,
    )));

    let subclass_reference = Rc::into_raw(Rc::clone(&state)).expose_provenance();

    // SAFETY: `window` is a valid window handle, `clipboard_subproc` is in the static memory,
    // and the subclass gets its own reference to the state, from `Rc::into_raw`.
    let winapi_result = unsafe { SetWindowSubclass(window, Some(clipboard_subproc), 0, subclass_reference) };

    if winapi_result == FALSE {
        // SAFETY: The reference comes from `Rc::into_raw` above and the subclass never got it.
        drop(unsafe {
            Rc::from_raw(core::ptr::with_exposed_provenance::<ClipboardWindowState>(
                subclass_reference,
            ))
        });
        return Err(WinCliprdrError::WindowSubclass);
    }

    Ok((window, state, backend_tx))
}

/// Windows-specific clipboard backend factory
struct WinCliprdrBackendFactory {
    tx: mpsc_sync::SyncSender<BackendEvent>,
    window: HWND,
    file_transfer: bool,
    shared: Arc<ClipboardShared>,
}

// SAFETY: window handle is thread safe for PostMessageW usage
unsafe impl Send for WinCliprdrBackendFactory {}

impl CliprdrBackendFactory for WinCliprdrBackendFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(WinCliprdrBackend::new(
            self.window,
            self.tx.clone(),
            self.file_transfer,
            Arc::clone(&self.shared),
        ))
    }
}
