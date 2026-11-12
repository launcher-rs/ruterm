#![cfg_attr(doc, doc = include_str!("../README.md"))]
#![doc(html_logo_url = "https://cdnweb.devolutions.net/images/projects/devolutions/logos/devolutions-icon-shadow.svg")]
#![warn(unsafe_op_in_unsafe_fn)]
#![warn(invalid_reference_casting)]
#![warn(clippy::undocumented_unsafe_blocks)]
#![warn(clippy::multiple_unsafe_ops_per_block)]
#![warn(clippy::transmute_ptr_to_ptr)]
#![warn(clippy::as_ptr_cast_mut)]
#![warn(clippy::cast_ptr_alignment)]
#![warn(clippy::fn_to_numeric_cast_any)]
#![warn(clippy::ptr_cast_constness)]

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use crate::windows::{HWND, WinClipboard, WinCliprdrError, WinCliprdrResult};

/// 界面侧的上传进度登记（本机副本新增）：写剪贴板前登记，剪贴板线程办理时回报字节数。
pub mod upload;

mod stub;
use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// RuTerm: process-wide clipboard authorization.
///
/// Every RDP session gets its own token; only the token the embedder last authorized may
/// read or write the system clipboard. Without this every connected RDP session would
/// broadcast the local clipboard (including copied files) to its own remote at once.
static NEXT_CLIPBOARD_SESSION: AtomicU64 = AtomicU64::new(1);
static ACTIVE_CLIPBOARD_SESSION: AtomicU64 = AtomicU64::new(0);
thread_local! {
    static PENDING_CLIPBOARD_SESSION: Cell<u64> = const { Cell::new(0) };
}

/// Allocate a token for a session before the clipboard backend is built on this thread.
pub fn allocate_clipboard_session() -> u64 {
    NEXT_CLIPBOARD_SESSION.fetch_add(1, Ordering::Relaxed)
}

/// Hand the token to the `WinClipboard` this thread is about to create.
pub fn prepare_clipboard_session(token: u64) {
    PENDING_CLIPBOARD_SESSION.with(|pending| pending.set(token));
}

pub(crate) fn take_prepared_clipboard_session() -> u64 {
    PENDING_CLIPBOARD_SESSION.with(|pending| pending.replace(0))
}

/// Allow exactly one session to touch the system clipboard; `0` revokes all.
pub fn authorize_clipboard_session(token: u64) {
    ACTIVE_CLIPBOARD_SESSION.store(token, Ordering::Release);
}

/// Revoke `token` only if it is still the active one (a newer tab may have taken over).
pub fn release_clipboard_session(token: u64) {
    let _ = ACTIVE_CLIPBOARD_SESSION.compare_exchange(
        token,
        0,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

pub(crate) fn is_clipboard_session_authorized(token: u64) -> bool {
    token != 0 && ACTIVE_CLIPBOARD_SESSION.load(Ordering::Acquire) == token
}

pub use crate::stub::{StubClipboard, StubCliprdrBackend};

/// Process-wide monotonic clock epoch for `CliprdrBackend::now_ms()` on native platforms.
///
/// Uses a lazily-initialized `Instant` so that all backends in the same process
/// share the same zero-point, producing comparable timestamps.
fn epoch() -> &'static Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now)
}

/// Returns monotonic milliseconds since process start, for use by native
/// `CliprdrBackend` implementations.
pub fn native_now_ms() -> u64 {
    u64::try_from(epoch().elapsed().as_millis()).unwrap_or(u64::MAX)
}
