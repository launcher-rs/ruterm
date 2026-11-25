//! RuTerm 补丁：默认要求系统可信证书且主机名匹配；用户在连接表单显式勾选后，
//! 可仅对当前连接线程放宽校验（自签名 / 主机名不符的内网 RDP服务器）。
#![doc(html_logo_url = "https://cdnweb.devolutions.net/images/projects/devolutions/logos/devolutions-icon-shadow.svg")]

use std::cell::Cell;

// 每个 RDP 会话跑在自己的线程上，握手也在该线程完成；用线程局部状态避免
// 多个会话同时连接时，一个会话的"接受自签"影响到另一个会话。
thread_local! {
    static ACCEPT_INVALID_CERT: Cell<bool> = const { Cell::new(false) };
}

/// 仅对当前线程的下一次 TLS 握手生效：true = 接受自签名 / 主机名不符的证书。
pub fn set_accept_invalid_cert(accept: bool) {
    ACCEPT_INVALID_CERT.with(|flag| flag.set(accept));
}

pub(crate) fn accept_invalid_cert() -> bool {
    ACCEPT_INVALID_CERT.with(Cell::get)
}

#[cfg(feature = "rustls-no-provider")]
#[path = "rustls.rs"]
mod impl_;

#[cfg(feature = "native-tls")]
#[path = "native_tls.rs"]
mod impl_;

#[cfg(feature = "stub")]
#[path = "stub.rs"]
mod impl_;

#[cfg(any(
    not(any(feature = "stub", feature = "native-tls", feature = "rustls-no-provider")),
    all(feature = "stub", feature = "native-tls"),
    all(feature = "stub", feature = "rustls-no-provider"),
    all(feature = "rustls-no-provider", feature = "native-tls"),
))]
compile_error!(
    "a TLS backend must be selected by enabling a single feature out of: `rustls`, `native-tls`, `stub` (the rustls crypto provider is chosen via `rustls`/`rustls-aws-lc-rs`/`rustls-ring`/`rustls-no-provider`)"
);

// The whole public API of this crate.
#[cfg(any(feature = "stub", feature = "native-tls", feature = "rustls-no-provider"))]
pub use impl_::{TlsStream, negotiated, upgrade};

/// TLS parameters negotiated during the handshake, to the extent the active
/// backend exposes them.
///
/// The `rustls` backend reports both fields. The `native-tls` and `stub`
/// backends cannot introspect the negotiated parameters, so both are `None`
/// there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NegotiatedTls {
    /// Negotiated protocol version, e.g. `"TLSv1_3"`.
    pub version: Option<String>,
    /// Negotiated cipher suite, e.g. `"TLS13_AES_256_GCM_SHA384"`.
    pub cipher_suite: Option<String>,
}

pub fn extract_tls_server_public_key(cert: &x509_cert::Certificate) -> Option<&[u8]> {
    cert.tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
}
