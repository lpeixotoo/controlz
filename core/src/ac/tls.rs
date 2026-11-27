//! Legacy TLS profile for the LG Wi-Fi module (KB: AC — TLS / certificates).
//!
//! The module speaks TLS 1.2 with CBC-SHA suites only and presents a shared,
//! self-signed RSA-1024 certificate. Both profiles here are TLS 1.2 only at
//! `@SECLEVEL=0`. Our servers offer only [`SERVER_CIPHERS`] (ECDHE) and refuse
//! renegotiation; the setup client keeps the wider [`MODULE_CIPHERS`] and
//! legacy renegotiation. They are meant for module traffic only; never use
//! them for anything else.

use std::pin::Pin;
use std::time::Duration;

use openssl::pkey::{PKey, Private};
use openssl::ssl::{
    Ssl, SslAcceptor, SslAcceptorBuilder, SslConnector, SslMethod, SslOptions, SslVerifyMode,
    SslVersion,
};
use openssl::x509::X509;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_openssl::SslStream;

use crate::error::{ControlError, Result};

/// The cipher list the module accepts (probe: it negotiated `ECDHE-RSA-AES256-SHA`).
pub const MODULE_CIPHERS: &str =
    "ECDHE-RSA-AES256-SHA:ECDHE-RSA-AES128-SHA:AES256-SHA:AES128-SHA@SECLEVEL=0";

/// The cipher list our servers (local cloud HTTPS and MQTTS) offer: the
/// ECDHE subset of [`MODULE_CIPHERS`], so every session has forward secrecy.
///
/// Fallback until T1.9 confirms this on the device: if the module refuses
/// it, switch `server_acceptor` to [`MODULE_CIPHERS`] and record why in the KB.
pub const SERVER_CIPHERS: &str = "ECDHE-RSA-AES256-SHA:ECDHE-RSA-AES128-SHA@SECLEVEL=0";

/// Upper bound for one TLS handshake, so a stalled peer cannot block forever.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// `SSL_OP_LEGACY_SERVER_CONNECT`: connect to servers without RFC 5746
/// support. OpenSSL 3 dropped it from `SSL_OP_ALL` and the `openssl` crate
/// has no named flag for it.
const LEGACY_SERVER_CONNECT: SslOptions =
    SslOptions::from_bits_retain(openssl_sys::SSL_OP_LEGACY_SERVER_CONNECT);

/// A server-side TLS acceptor for the module (local cloud HTTPS and MQTTS).
///
/// TLS 1.2 only, [`SERVER_CIPHERS`], and renegotiation refused
/// (`NO_RENEGOTIATION`). Fallback until T1.9 confirms this on the device: if
/// the module needs renegotiation, set
/// `SslOptions::ALLOW_UNSAFE_LEGACY_RENEGOTIATION` instead (as T1.2 did) and
/// record why in the KB.
///
/// # Errors
/// [`ControlError::Invalid`] if OpenSSL rejects the profile, the certificate
/// or the key (for example a key that does not match the certificate).
pub fn server_acceptor(cert: &X509, key: &PKey<Private>) -> Result<SslAcceptor> {
    Ok(server_builder(cert, key)?.build())
}

/// The configured builder behind [`server_acceptor`]; split out so tests can
/// read its options (`SslContextRef` has no options getter).
fn server_builder(cert: &X509, key: &PKey<Private>) -> Result<SslAcceptorBuilder> {
    let mut builder =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).map_err(invalid)?;
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(invalid)?;
    builder
        .set_max_proto_version(Some(SslVersion::TLS1_2))
        .map_err(invalid)?;
    builder.set_cipher_list(SERVER_CIPHERS).map_err(invalid)?;
    builder.set_options(SslOptions::NO_RENEGOTIATION);
    builder.set_certificate(cert).map_err(invalid)?;
    builder.set_private_key(key).map_err(invalid)?;
    builder.check_private_key().map_err(invalid)?;
    Ok(builder)
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

    /// A client like the module: TLS 1.2 with `ECDHE-RSA-AES256-SHA` only.
    fn module_like_client() -> SslConnector {
        client(|b| {
            b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_cipher_list("ECDHE-RSA-AES256-SHA@SECLEVEL=0")
                .unwrap();
        })
    }

    type ClientResult = std::result::Result<SslStream<tokio::io::DuplexStream>, String>;

    /// Handshakes over an in-memory pipe; returns (client, server) results.
    async fn handshake(
        connector: SslConnector,
    ) -> (ClientResult, Result<SslStream<tokio::io::DuplexStream>>) {
        handshake_with(connector, &acceptor()).await
    }

    async fn handshake_with(
        connector: SslConnector,
        acceptor: &SslAcceptor,
    ) -> (ClientResult, Result<SslStream<tokio::io::DuplexStream>>) {
        let (c, s) = tokio::io::duplex(16 * 1024);
        let client = async move {
            let ssl = connector.configure().unwrap().into_ssl(HOSTNAME).unwrap();
            let mut tls = SslStream::new(ssl, c).unwrap();
            match Pin::new(&mut tls).connect().await {
                Ok(()) => Ok(tls),
                Err(e) => Err(e.to_string()),
            }
        };
        tokio::join!(client, accept(acceptor, s))
    }

    /// Test-only binding: `openssl-sys` has no `SSL_renegotiate`. The symbol
    /// comes from the vendored libssl that `openssl-sys` already links.
    #[allow(unsafe_code)]
    mod ffi {
        unsafe extern "C" {
            pub(super) fn SSL_renegotiate(s: *mut openssl_sys::SSL) -> std::ffi::c_int;
        }
    }

    /// Asks OpenSSL to start a client-side renegotiation on the next handshake step.
    #[allow(unsafe_code)]
    fn request_renegotiation(ssl: &openssl::ssl::SslRef) {
        let ptr = std::ptr::from_ref(ssl)
            .cast::<openssl_sys::SSL>()
            .cast_mut();
        // SAFETY: an `&SslRef` is, by the foreign-types design the openssl
        // crate uses, a pointer to the live C `SSL` object (its Rust type is
        // an opaque `UnsafeCell` wrapper, so C-side mutation through it is
        // allowed). The stream that owns it outlives this call, and nothing
        // else uses it concurrently (single task, no await in between).
        let rc = unsafe { ffi::SSL_renegotiate(ptr) };
        assert_eq!(rc, 1, "SSL_renegotiate refused to start");
    }

    /// Completes a handshake, then has the client renegotiate while the
    /// server reads. Returns the client's renegotiation result.
    async fn client_renegotiates(acceptor: &SslAcceptor) -> std::result::Result<(), String> {
        use tokio::io::AsyncReadExt as _;
        let (client, server) = handshake_with(module_like_client(), acceptor).await;
        let mut client = client.expect("client handshake");
        let mut server = server.expect("server handshake");
        let reader = tokio::spawn(async move {
            let mut buf = [0_u8; 64];
            let _ = server.read(&mut buf).await;
        });
        request_renegotiation(client.ssl());
        let result =
            tokio::time::timeout(Duration::from_secs(5), Pin::new(&mut client).do_handshake())
                .await
                .expect("renegotiation must not hang")
                .map_err(|e| e.to_string());
        reader.abort();
        result
    }

    #[tokio::test]
    async fn module_cipher_handshakes() {
        let (client, server) = handshake(module_like_client()).await;
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

    #[tokio::test]
    async fn non_ecdhe_client_is_refused() {
        let connector = client(|b| {
            b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_cipher_list("AES256-SHA@SECLEVEL=0").unwrap();
        });
        let (client, server) = handshake(connector).await;
        assert!(
            client.is_err(),
            "a client offering only AES256-SHA must fail"
        );
        assert!(
            matches!(server, Err(ControlError::Network { .. })),
            "server must refuse non-ECDHE suites"
        );
    }

    #[tokio::test]
    async fn client_renegotiation_is_refused() {
        let refused = client_renegotiates(&acceptor()).await;
        assert!(refused.is_err(), "server must refuse renegotiation");
        // OpenSSL 3 refuses client renegotiation even with the T1.2 flag, so
        // the handshake alone cannot tell the profiles apart; check the options.
        let builder = server_builder(FIXTURE.server.cert(), FIXTURE.server.key()).unwrap();
        let options = builder.options();
        assert!(options.contains(SslOptions::NO_RENEGOTIATION));
        assert!(!options.contains(SslOptions::ALLOW_UNSAFE_LEGACY_RENEGOTIATION));
    }

    /// Control for `client_renegotiation_is_refused`: the same client does
    /// renegotiate with a server that allows it, so the harness really
    /// renegotiates. OpenSSL 3 servers refuse client renegotiation unless
    /// `SSL_OP_ALLOW_CLIENT_RENEGOTIATION` (bit 8, `ssl.h`; no binding in
    /// `openssl-sys`) is set and `NO_RENEGOTIATION` is not
    /// (`RENEG_OPTIONS_OK`, `ssl/statem/statem_srvr.c`).
    #[tokio::test]
    async fn renegotiation_works_when_allowed() {
        const ALLOW_CLIENT_RENEGOTIATION: SslOptions = SslOptions::from_bits_retain(1 << 8);
        let mut b = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).unwrap();
        b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        b.set_cipher_list(SERVER_CIPHERS).unwrap();
        b.set_options(ALLOW_CLIENT_RENEGOTIATION);
        b.set_certificate(FIXTURE.server.cert()).unwrap();
        b.set_private_key(FIXTURE.server.key()).unwrap();
        client_renegotiates(&b.build())
            .await
            .expect("renegotiation with a permissive server");
    }
}
