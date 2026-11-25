use std::io;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};

pub type TlsStream<S> = tokio_native_tls::TlsStream<S>;

pub async fn upgrade<S>(stream: S, server_name: &str) -> io::Result<(TlsStream<S>, x509_cert::Certificate)>
where
    S: Unpin + AsyncRead + AsyncWrite,
{
    let mut tls_stream = {
        let accept_invalid = crate::accept_invalid_cert();
        let mut builder = tokio_native_tls::native_tls::TlsConnector::builder();
        // 默认保留系统 CA 与主机名校验；只有用户在表单显式勾选时才放宽，
        // 且只影响当前会话线程，不会把后续连接一起降级。
        if accept_invalid {
            builder
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true);
        }
        let connector = builder
            .use_sni(true)
            .build()
            .map(tokio_native_tls::TlsConnector::from)
            .map_err(io::Error::other)?;

        connector.connect(server_name, stream).await.map_err(|error| {
            if accept_invalid {
                io::Error::other(format!("TLS 握手失败：{error}"))
            } else {
                io::Error::other(format!("TLS 验证/握手失败：{error}；请安装系统信任且主机名匹配的证书，或在连接表单勾选「接受自签名证书（不安全）」"))
            }
        })?
    };

    tls_stream.flush().await?;

    let tls_cert = {
        use x509_cert::der::Decode as _;

        let cert = tls_stream
            .get_ref()
            .peer_certificate()
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("peer certificate is missing"))?;
        let cert = cert.to_der().map_err(io::Error::other)?;

        x509_cert::Certificate::from_der(&cert).map_err(io::Error::other)?
    };

    Ok((tls_stream, tls_cert))
}

/// The `native-tls` backend does not expose the negotiated version or cipher.
pub fn negotiated<S>(_stream: &TlsStream<S>) -> crate::NegotiatedTls {
    crate::NegotiatedTls::default()
}
