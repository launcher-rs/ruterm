use std::io;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::ServerName;

pub type TlsStream<S> = tokio_rustls::client::TlsStream<S>;

pub async fn upgrade<S>(stream: S, server_name: &str) -> io::Result<(TlsStream<S>, x509_cert::Certificate)>
where
    S: Unpin + AsyncRead + AsyncWrite,
{
    // RuTerm: use the OS trust store and rustls' normal chain/name/signature verifier.
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    for cert in loaded.certs {
        roots.add(cert).map_err(io::Error::other)?;
    }
    if roots.is_empty() {
        return Err(io::Error::other("系统可信 CA 为空；请安装/使用受信证书"));
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // CredSSP does not support TLS resumption.
    config.resumption = rustls::client::Resumption::disabled();
    let domain = ServerName::try_from(server_name.to_owned()).map_err(io::Error::other)?;
    let mut tls_stream = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(domain, stream).await.map_err(|error| {
            io::Error::other(format!("TLS 验证/握手失败：{error}；请安装/使用系统信任且主机名匹配的证书，不接受未受信自签证书"))
        })?;
    tls_stream.flush().await?;
    let tls_cert = {
        use x509_cert::der::Decode as _;
        let cert = tls_stream.get_ref().1.peer_certificates()
            .and_then(|certificates| certificates.first())
            .ok_or_else(|| io::Error::other("peer certificate is missing"))?;
        x509_cert::Certificate::from_der(cert).map_err(io::Error::other)?
    };
    Ok((tls_stream, tls_cert))
}

pub fn negotiated<S>(stream: &TlsStream<S>) -> crate::NegotiatedTls {
    let (_, connection) = stream.get_ref();
    crate::NegotiatedTls {
        version: connection.protocol_version().map(|version| format!("{version:?}")),
        cipher_suite: connection.negotiated_cipher_suite().map(|suite| format!("{:?}", suite.suite())),
    }
}
