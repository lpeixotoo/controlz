//! Legacy TLS profile for the LG Wi-Fi module (KB: AC — TLS / certificates).
//!
//! The module speaks TLS 1.2 with CBC-SHA suites only and presents a shared,
//! self-signed RSA-1024 certificate. Both profiles here are TLS 1.2 only, use
//! [`MODULE_CIPHERS`] at `@SECLEVEL=0` and allow legacy renegotiation. They are
//! meant for module traffic only; never use them for anything else.

use std::pin::Pin;
use std::time::Duration;

use openssl::pkey::{PKey, Private};
use openssl::ssl::{
    Ssl, SslAcceptor, SslConnector, SslMethod, SslOptions, SslVerifyMode, SslVersion,
};
use openssl::x509::X509;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_openssl::SslStream;

use crate::error::{ControlError, Result};

/// The cipher list the module accepts (probe: it negotiated `ECDHE-RSA-AES256-SHA`).
pub const MODULE_CIPHERS: &str =
    "ECDHE-RSA-AES256-SHA:ECDHE-RSA-AES128-SHA:AES256-SHA:AES128-SHA@SECLEVEL=0";

/// Upper bound for one TLS handshake, so a stalled peer cannot block forever.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// `SSL_OP_LEGACY_SERVER_CONNECT`: connect to servers without RFC 5746
/// support. OpenSSL 3 dropped it from `SSL_OP_ALL` and the `openssl` crate
/// has no named flag for it.
const LEGACY_SERVER_CONNECT: SslOptions =
    SslOptions::from_bits_retain(openssl_sys::SSL_OP_LEGACY_SERVER_CONNECT);

/// A server-side TLS acceptor for the module (local cloud HTTPS and MQTTS).
///
/// # Errors
/// [`ControlError::Invalid`] if OpenSSL rejects the profile, the certificate
/// or the key (for example a key that does not match the certificate).
pub fn server_acceptor(cert: &X509, key: &PKey<Private>) -> Result<SslAcceptor> {
    let mut builder =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).map_err(invalid)?;
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(invalid)?;
    builder
        .set_max_proto_version(Some(SslVersion::TLS1_2))
        .map_err(invalid)?;
    builder.set_cipher_list(MODULE_CIPHERS).map_err(invalid)?;
    builder.set_options(SslOptions::ALLOW_UNSAFE_LEGACY_RENEGOTIATION);
    builder.set_certificate(cert).map_err(invalid)?;
    builder.set_private_key(key).map_err(invalid)?;
    builder.check_private_key().map_err(invalid)?;
    Ok(builder.build())
}

/// A client-side TLS connector for the module's setup port.
///
/// Certificate verification is off: the module's certificate is self-signed
/// and the same on every device, so it proves nothing.
///
/// # Errors
/// [`ControlError::Invalid`] if OpenSSL rejects the profile.
pub fn module_connector() -> Result<SslConnector> {
    let mut builder = SslConnector::builder(SslMethod::tls_client()).map_err(invalid)?;
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(invalid)?;
    builder
        .set_max_proto_version(Some(SslVersion::TLS1_2))
        .map_err(invalid)?;
    builder.set_cipher_list(MODULE_CIPHERS).map_err(invalid)?;
    builder.set_options(SslOptions::ALLOW_UNSAFE_LEGACY_RENEGOTIATION | LEGACY_SERVER_CONNECT);
    builder.set_verify(SslVerifyMode::NONE);
    Ok(builder.build())
}

/// Runs the server side of the TLS handshake on `stream`, bounded by
/// [`HANDSHAKE_TIMEOUT`].
///
/// # Errors
/// [`ControlError::Network`] if the handshake fails or times out.
pub async fn accept<S>(acceptor: &SslAcceptor, stream: S) -> Result<SslStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ssl = Ssl::new(acceptor.context()).map_err(ControlError::network)?;
    let mut tls = SslStream::new(ssl, stream).map_err(ControlError::network)?;
    tokio::time::timeout(HANDSHAKE_TIMEOUT, Pin::new(&mut tls).accept())
        .await
        .map_err(|_| ControlError::network("TLS accept timed out"))?
        .map_err(|e| ControlError::network(format!("TLS accept failed: {e}")))?;
    Ok(tls)
}

/// Runs the client side of the TLS handshake on `stream`, bounded by
/// [`HANDSHAKE_TIMEOUT`].
///
/// `domain` is sent as SNI (skipped by OpenSSL for IP addresses); the
/// hostname is not verified (see [`module_connector`]).
///
/// # Errors
/// [`ControlError::Network`] if the handshake fails or times out.
pub async fn connect<S>(connector: &SslConnector, domain: &str, stream: S) -> Result<SslStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ssl = connector
        .configure()
        .map_err(ControlError::network)?
        .verify_hostname(false)
        .into_ssl(domain)
        .map_err(ControlError::network)?;
    let mut tls = SslStream::new(ssl, stream).map_err(ControlError::network)?;
    tokio::time::timeout(HANDSHAKE_TIMEOUT, Pin::new(&mut tls).connect())
        .await
        .map_err(|_| ControlError::network("TLS connect timed out"))?
        .map_err(|e| ControlError::network(format!("TLS connect failed: {e}")))?;
    Ok(tls)
}

#[allow(clippy::needless_pass_by_value)] // used as a `map_err` callback
fn invalid(e: openssl::error::ErrorStack) -> ControlError {
    ControlError::invalid(format!("TLS setup: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ac::pki::test_fixture::{FIXTURE, HOSTNAME};
    use openssl::ssl::SslConnectorBuilder;

    /// A client like the module: TLS 1.2, one CBC suite, verifying our CA and SAN.
    fn client(builder: impl FnOnce(&mut SslConnectorBuilder)) -> SslConnector {
        let mut b = SslConnector::builder(SslMethod::tls_client()).unwrap();
        b.cert_store_mut()
            .add_cert(FIXTURE.ca.cert().clone())
            .unwrap();
        builder(&mut b);
        b.build()
    }

    fn acceptor() -> SslAcceptor {
        server_acceptor(FIXTURE.server.cert(), FIXTURE.server.key()).unwrap()
    }

    /// Handshakes over an in-memory pipe; returns (client, server) results.
    async fn handshake(
        connector: SslConnector,
    ) -> (
        std::result::Result<SslStream<tokio::io::DuplexStream>, String>,
        Result<SslStream<tokio::io::DuplexStream>>,
    ) {
        let (c, s) = tokio::io::duplex(16 * 1024);
        let acceptor = acceptor();
        let client = async move {
            let ssl = connector.configure().unwrap().into_ssl(HOSTNAME).unwrap();
            let mut tls = SslStream::new(ssl, c).unwrap();
            match Pin::new(&mut tls).connect().await {
                Ok(()) => Ok(tls),
                Err(e) => Err(e.to_string()),
            }
        };
        tokio::join!(client, accept(&acceptor, s))
    }

    #[tokio::test]
    async fn module_cipher_handshakes() {
        let connector = client(|b| {
            b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_cipher_list("ECDHE-RSA-AES256-SHA@SECLEVEL=0")
                .unwrap();
        });
        let (client, server) = handshake(connector).await;
        let client = client.expect("client handshake");
        let server = server.expect("server handshake");
        for ssl in [client.ssl(), server.ssl()] {
            assert_eq!(ssl.version_str(), "TLSv1.2");
            assert_eq!(
                ssl.current_cipher().map(openssl::ssl::SslCipherRef::name),
                Some("ECDHE-RSA-AES256-SHA")
            );
        }
    }

    #[tokio::test]
    async fn tls13_only_client_is_refused() {
        let connector = client(|b| {
            b.set_min_proto_version(Some(SslVersion::TLS1_3)).unwrap();
            b.set_max_proto_version(Some(SslVersion::TLS1_3)).unwrap();
        });
        let (client, server) = handshake(connector).await;
        assert!(client.is_err(), "TLS 1.3 client must fail");
        assert!(
            matches!(server, Err(ControlError::Network { .. })),
            "server must refuse TLS 1.3"
        );
    }
}
