//! Shared TLS connector for STT streaming WebSocket upstreams.
//!
//! `tokio_tungstenite::connect_async` relies on rustls's process-level default
//! `CryptoProvider`, which is never installed in this workspace — a real
//! `wss://` connect panics with "Could not automatically determine the
//! process-level CryptoProvider". Mirrors the explicit-connector pattern
//! established by the lark/dingtalk channel plugins
//! (`aionui-channel/src/plugins/lark/plugin.rs`).

use std::sync::Arc;

use crate::error::SttError;

/// Build an explicit rustls connector for upstream WebSocket connections.
///
/// The connector is only consulted for `wss://` targets;
/// `connect_async_tls_with_config` ignores it for plain `ws://` URLs, so
/// callers can pass it unconditionally.
///
/// Explicitly sets ALPN to `http/1.1` only — WebSocket requires an HTTP/1.1
/// upgrade handshake and is incompatible with h2. Without this, some servers
/// negotiate h2 via ALPN and the WebSocket upgrade never completes.
pub(crate) fn build_ws_connector() -> Result<tokio_tungstenite::Connector, SttError> {
    let certs = rustls_native_certs::load_native_certs();
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add_parsable_certificates(certs.certs);

    build_ws_connector_with_roots(root_store)
}

fn build_ws_connector_with_roots(root_store: rustls::RootCertStore) -> Result<tokio_tungstenite::Connector, SttError> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));

    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| SttError::RequestFailed(format!("TLS config error: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::{Error, Message};

    const CERT: &[u8] = include_bytes!("../tests/fixtures/tls/localhost.der");
    const KEY: &[u8] = include_bytes!("../tests/fixtures/tls/localhost-key.der");

    async fn connect_loopback(roots: rustls::RootCertStore, hostname: &str) -> Result<(), Error> {
        let mut config =
            rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(CERT.to_vec())],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
                )
                .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let Ok(tls) = acceptor.accept(socket).await else {
                return false;
            };
            assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"http/1.1".as_slice()));
            let mut websocket = tokio_tungstenite::accept_async(tls).await.unwrap();
            let message = websocket.next().await.unwrap().unwrap();
            websocket.send(message).await.unwrap();
            true
        });
        let connector = build_ws_connector_with_roots(roots).unwrap();
        let connection = tokio_tungstenite::connect_async_tls_with_config(
            format!("wss://{hostname}:{port}/"),
            None,
            false,
            Some(connector),
        )
        .await;
        let result = match connection {
            Ok((mut websocket, _)) => {
                websocket.send(Message::Text("tls-regression".into())).await.unwrap();
                assert_eq!(
                    websocket.next().await.unwrap().unwrap(),
                    Message::Text("tls-regression".into())
                );
                Ok(())
            }
            Err(error) => Err(error),
        };
        assert_eq!(server.await.unwrap(), result.is_ok());
        result
    }

    fn trusted_roots() -> rustls::RootCertStore {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(CERT.to_vec())).unwrap();
        roots
    }

    fn assert_certificate_error(result: Result<(), Error>, expected: fn(&rustls::CertificateError) -> bool) {
        let Err(Error::Io(error)) = result else {
            panic!("expected TLS I/O error, got {result:?}");
        };
        let Some(rustls::Error::InvalidCertificate(certificate_error)) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<rustls::Error>())
        else {
            panic!("expected rustls certificate error, got {error:?}");
        };
        assert!(
            expected(certificate_error),
            "unexpected certificate error: {certificate_error:?}"
        );
    }

    #[tokio::test]
    async fn trusted_tls_websocket_echoes_with_http1_alpn() {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_loopback(trusted_roots(), "localhost"),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn untrusted_tls_certificate_is_rejected() {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_loopback(rustls::RootCertStore::empty(), "localhost"),
        )
        .await
        .unwrap();
        assert_certificate_error(result, |error| matches!(error, rustls::CertificateError::UnknownIssuer));
    }

    #[tokio::test]
    async fn tls_certificate_hostname_mismatch_is_rejected() {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_loopback(trusted_roots(), "127.0.0.1"),
        )
        .await
        .unwrap();
        assert_certificate_error(result, |error| {
            matches!(error, rustls::CertificateError::NotValidForNameContext { .. })
        });
    }

    #[test]
    fn connector_builds_with_rustls_tls() {
        match build_ws_connector() {
            Ok(tokio_tungstenite::Connector::Rustls(config)) => {
                assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
            }
            Ok(_) => panic!("expected a Rustls connector"),
            Err(e) => panic!("connector build failed: {e}"),
        }
    }
}
