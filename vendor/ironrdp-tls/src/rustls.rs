use std::io;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::ServerName;

pub type TlsStream<S> = tokio_rustls::client::TlsStream<S>;

pub async fn upgrade<S>(stream: S, server_name: &str) -> io::Result<(TlsStream<S>, x509_cert::Certificate)>
where
    S: Unpin + AsyncRead + AsyncWrite,
{
    let accept_invalid = crate::accept_invalid_cert();
    let mut config = if accept_invalid {
        // 用户明确勾选：证书链与主机名都不校验（仅当前会话线程）。
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(danger::NoCertificateVerification))
            .with_no_client_auth()
    } else {
        // 默认走系统信任库与正常的链 / 主机名 / 签名校验。
        let loaded = rustls_native_certs::load_native_certs();
        let mut roots = rustls::RootCertStore::empty();
        for cert in loaded.certs {
            roots.add(cert).map_err(io::Error::other)?;
        }
        if roots.is_empty() {
            return Err(io::Error::other("系统可信 CA 为空；请安装/使用受信证书"));
        }
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    // CredSSP does not support TLS resumption.
    config.resumption = rustls::client::Resumption::disabled();
    let domain = ServerName::try_from(server_name.to_owned()).map_err(io::Error::other)?;
    let mut tls_stream = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(domain, stream).await.map_err(|error| {
            if accept_invalid {
                io::Error::other(format!("TLS 握手失败：{error}"))
            } else {
                io::Error::other(format!("TLS 验证/握手失败：{error}；请安装系统信任且主机名匹配的证书，或在连接表单勾选「接受自签名证书（不安全）」"))
            }
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

mod danger {
    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::pki_types;
    use tokio_rustls::rustls::{DigitallySignedStruct, Error, SignatureScheme};

    #[derive(Debug)]
    pub(super) struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _: &pki_types::CertificateDer<'_>,
            _: &[pki_types::CertificateDer<'_>],
            _: &pki_types::ServerName<'_>,
            _: &[u8],
            _: pki_types::UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ED25519,
                SignatureScheme::ED448,
            ]
        }
    }
}
