#![cfg(feature = "__rustls-used")]

/// RuTerm 本地补丁：加载系统根证书，供 CredSSP 的 TLS 校验使用。
///
/// 上游 CredSSP 用 `NoCertificateVerification` 接受任意证书；RuTerm 改为正常校验，
/// 需要系统可信 CA。加载失败（没有任何可信根）时返回 None，由调用方拒绝连接。
#[cfg(feature = "rustls-native-certs")]
pub(crate) fn load_native_root_store() -> Option<rustls::RootCertStore> {
    let result = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    for cert in result.certs {
        let _ = roots.add(cert);
    }
    (!roots.is_empty()).then_some(roots)
}

/// 非 Windows 或未启用 native-certs 时回退：rustls 无内置根，返回 None。
#[cfg(not(feature = "rustls-native-certs"))]
pub(crate) fn load_native_root_store() -> Option<rustls::RootCertStore> {
    None
}

/// Call this before using rustls.
#[doc(hidden)]
#[allow(clippy::result_unit_err)]
pub fn install_default_crypto_provider_if_necessary() -> Result<(), ()> {
    #[cfg(feature = "__install-crypto-provider")]
    {
        static INSTALL: std::sync::OnceLock<Result<(), ()>> = std::sync::OnceLock::new();

        let result = INSTALL.get_or_init(|| {
            // A crypto provider is already installed.
            if rustls::crypto::CryptoProvider::get_default().is_some() {
                return Ok(());
            }

            #[cfg(feature = "aws-lc-rs")]
            {
                rustls::crypto::aws_lc_rs::default_provider()
                    .install_default()
                    .map_err(|_| ())
            }

            #[cfg(all(not(feature = "aws-lc-rs"), feature = "ring"))]
            {
                rustls::crypto::ring::default_provider()
                    .install_default()
                    .map_err(|_| ())
            }
        });

        *result
    }

    #[cfg(not(feature = "__install-crypto-provider"))]
    {
        Ok(())
    }
}

#[cfg(feature = "network_client")]
pub(crate) fn load_native_certs(builder: reqwest::blocking::ClientBuilder) -> reqwest::blocking::ClientBuilder {
    #[cfg(feature = "aws-lc-rs")]
    {
        let mut builder = builder;

        let result = rustls_native_certs::load_native_certs();

        for error in result.errors {
            debug!(%error, "native root CA certificate loading error");
        }

        for cert in result.certs {
            // Continue on parsing errors, as native stores often include ancient or syntactically
            // invalid certificates, like root certificates without any X509 extensions.
            // Inspiration: https://github.com/rustls/rustls/blob/633bf4ba9d9521a95f68766d04c22e2b01e68318/rustls/src/anchors.rs#L105-L112
            match reqwest::Certificate::from_der(&cert) {
                Ok(cert) => builder = builder.add_root_certificate(cert),
                Err(error) => {
                    debug!(%error, "failed to parse native certificate");
                }
            };
        }

        builder
    }

    // We enable the rustls-tls-native-roots feature of reqwest when ring is used.
    #[cfg(all(not(feature = "aws-lc-rs"), feature = "ring"))]
    {
        builder
    }
}
