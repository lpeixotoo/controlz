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
use openssl::stack::Stack;
use openssl::stack::StackRef;
use openssl::x509::store::X509StoreBuilder;
use openssl::x509::{X509, X509Name, X509PurposeId, X509Ref, X509StoreContext};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_openssl::SslStream;

use crate::error::{ControlError, Result};

/// The cipher list the module accepts (probe: it negotiated `ECDHE-RSA-AES256-SHA`).
pub const MODULE_CIPHERS: &str =
    "ECDHE-RSA-AES256-SHA:ECDHE-RSA-AES128-SHA:AES256-SHA:AES128-SHA@SECLEVEL=0";

/// The cipher list our servers (local cloud HTTPS and MQTTS) offer: the
/// ECDHE subset of [`MODULE_CIPHERS`], so every session has forward secrecy.
///
/// Confirmed on the device (T1.9): the AC connected on 443 and 8883 with no
/// fallback (the exact suite was not logged). Should a module ever refuse
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
/// (`NO_RENEGOTIATION`), confirmed on the device (T1.9): the AC never needed
/// renegotiation. Should a module ever need it, set
/// `SslOptions::ALLOW_UNSAFE_LEGACY_RENEGOTIATION` instead (as T1.2 did) and
/// record why in the KB.
///
/// # Errors
/// [`ControlError::Invalid`] if OpenSSL rejects the profile, the certificate
/// or the key (for example a key that does not match the certificate).
pub fn server_acceptor(cert: &X509, key: &PKey<Private>) -> Result<SslAcceptor> {
    Ok(server_builder(cert, key)?.build())
}

/// A server-side TLS acceptor for the module's MQTTS connection: the
/// [`server_acceptor`] profile plus a mandatory client certificate (D21).
///
/// Clients without a certificate fail the handshake, and TLS still checks
/// the client's `CertificateVerify`, so a peer must hold the private key of
/// the certificate it presents. Which certificates pass:
/// - one that verifies against `client_ca` (the store holds only that CA)
///   with OpenSSL's `sslclient` purpose: the CSR path, unchanged;
/// - one from any other issuer, as long as the only verification errors are
///   "this chain does not lead to a CA we trust" ([`foreign_chain_error`]).
///   The broker then accepts it only by its pinned public key.
///
/// A certificate whose leaf names `client_ca` or `server_ca` as issuer gets
/// no such leniency: from our own PKI, it must verify fully (so a server
/// certificate or one without clientAuth still fails here). Anything else
/// (bad signature inside the chain, wrong purpose, malformed) fails too.
/// `client_ca`'s name is the only entry of the CA list sent in
/// `CertificateRequest`.
///
/// # Errors
/// [`ControlError::Invalid`] if OpenSSL rejects the profile, the certificate,
/// the key or a CA.
pub fn mqtt_acceptor(
    cert: &X509,
    key: &PKey<Private>,
    client_ca: &X509,
    server_ca: &X509,
) -> Result<SslAcceptor> {
    let mut builder = server_builder(cert, key)?;
    let mut store = X509StoreBuilder::new().map_err(invalid)?;
    store.add_cert(client_ca.clone()).map_err(invalid)?;
    store
        .set_purpose(X509PurposeId::SSL_CLIENT)
        .map_err(invalid)?;
    builder
        .set_verify_cert_store(store.build())
        .map_err(invalid)?;
    let mut names = Stack::new().map_err(invalid)?;
    names
        .push(client_ca.subject_name().to_owned().map_err(invalid)?)
        .map_err(invalid)?;
    builder.set_client_ca_list(names);
    // Room for a foreign chain the module may send whole; our own client
    // certificates are held to one level by `chains_to_client_ca`.
    builder.set_verify_depth(MQTT_VERIFY_DEPTH);
    let ours = our_ca_names(&[client_ca, server_ca])?;
    builder.set_verify_callback(
        SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT,
        move |preverified, ctx| {
            if preverified {
                return true;
            }
            let error = ctx.error();
            let leaf_is_ours = ctx
                .chain()
                .and_then(|chain| chain.iter().next())
                .is_none_or(|leaf| issued_by_any(leaf, &ours));
            if !leaf_is_ours && foreign_chain_error(error.as_raw()) {
                return true;
            }
            // Warn: shown at the CLI's default verbosity.
            tracing::warn!(
                depth = ctx.error_depth(),
                error = error.error_string(),
                "mqtt: client certificate refused"
            );
            false
        },
    );
    Ok(builder.build())
}

/// Verify depth of [`mqtt_acceptor`]: leaf plus up to four CA levels.
const MQTT_VERIFY_DEPTH: u32 = 4;

/// Verification errors that only say the peer's chain does not end at a CA
/// we trust, which is expected for the module's LG-issued certificate (D21):
/// no local issuer, self-signed leaf or root, unverifiable leaf, untrusted.
///
/// Validity dates are tolerated as well (`CERT_HAS_EXPIRED`,
/// `CERT_NOT_YET_VALID`): the dates are an assertion of an issuer we do not
/// trust anyway, the pinned key is what we rely on, and a module from its
/// factory registration must not lock itself out when LG's certificate ages
/// out. Our own client certificates never get this leniency (their issuer
/// is ours, see [`mqtt_acceptor`]).
fn foreign_chain_error(raw: std::ffi::c_int) -> bool {
    matches!(
        raw,
        openssl_sys::X509_V_ERR_UNABLE_TO_GET_ISSUER_CERT
            | openssl_sys::X509_V_ERR_UNABLE_TO_GET_ISSUER_CERT_LOCALLY
            | openssl_sys::X509_V_ERR_DEPTH_ZERO_SELF_SIGNED_CERT
            | openssl_sys::X509_V_ERR_SELF_SIGNED_CERT_IN_CHAIN
            | openssl_sys::X509_V_ERR_UNABLE_TO_VERIFY_LEAF_SIGNATURE
            | openssl_sys::X509_V_ERR_CERT_UNTRUSTED
            | openssl_sys::X509_V_ERR_CERT_HAS_EXPIRED
            | openssl_sys::X509_V_ERR_CERT_NOT_YET_VALID
    )
}

/// Owned subject names of `cas`, for [`issued_by_any`].
///
/// # Errors
/// [`ControlError::Invalid`] if OpenSSL cannot copy a name.
pub(crate) fn our_ca_names(cas: &[&X509]) -> Result<Vec<X509Name>> {
    cas.iter()
        .map(|ca| ca.subject_name().to_owned().map_err(invalid))
        .collect()
}

/// Whether `cert` names one of `issuers` as its issuer (or cannot be compared).
pub(crate) fn issued_by_any(cert: &X509Ref, issuers: &[X509Name]) -> bool {
    issuers.iter().any(|name| {
        cert.issuer_name()
            .try_cmp(name)
            .map_or(true, |order| order == std::cmp::Ordering::Equal)
    })
}

/// Whether `leaf` is a client certificate issued directly by `client_ca`
/// for the `sslclient` purpose: the full check [`mqtt_acceptor`] applied
/// before D21, repeated after the handshake to tell CSR-issued
/// certificates apart from pinned ones. `chain` is the peer's
/// intermediates (untrusted).
pub(crate) fn chains_to_client_ca(
    leaf: &X509Ref,
    chain: Option<&StackRef<X509>>,
    client_ca: &X509,
) -> bool {
    let check = || -> std::result::Result<bool, openssl::error::ErrorStack> {
        let mut store = X509StoreBuilder::new()?;
        store.add_cert(client_ca.clone())?;
        store.set_purpose(X509PurposeId::SSL_CLIENT)?;
        let store = store.build();
        let mut untrusted = Stack::new()?;
        for cert in chain.into_iter().flatten() {
            untrusted.push(cert.to_owned())?;
        }
        let mut ctx = X509StoreContext::new()?;
        ctx.init(&store, leaf, &untrusted, |ctx| {
            let ok = ctx.verify_cert()?;
            Ok(ok && ctx.chain().is_some_and(|c| c.len() == 2))
        })
    };
    check().unwrap_or(false)
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
    use crate::ac::pki::test_fixture::{
        FIXTURE, FOREIGN_CA, HOSTNAME, csr_pem, mismatched_rsa_key, p256_key,
    };
    use crate::ac::pki::{Ca, Identity};
    use crate::secret::Secret;
    use openssl::nid::Nid;
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

    // ---- mqtt_acceptor: mandatory client certificate ----

    const MQTT_UUID: &str = "00000000-fake-0000-0000-000000000000";

    /// An EC client certificate for `cn`, signed by `ca` (clientAuth EKU).
    fn client_identity(ca: &Ca, cn: &str) -> Identity {
        let key = p256_key();
        let pem = ca.sign_csr(&csr_pem(&key, &[cn]), cn, 30).unwrap();
        let key_pem =
            Secret::new(String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap());
        Identity::from_pem(&pem, &key_pem).unwrap()
    }

    fn mqtt_server() -> SslAcceptor {
        mqtt_acceptor(
            FIXTURE.server.cert(),
            FIXTURE.server.key(),
            FIXTURE.client_ca.cert(),
            FIXTURE.ca.cert(),
        )
        .unwrap()
    }

    /// A module-like client presenting `identity`, if any.
    fn mqtt_client(identity: Option<&Identity>) -> SslConnector {
        client(|b| {
            b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            b.set_cipher_list("ECDHE-RSA-AES256-SHA@SECLEVEL=0")
                .unwrap();
            if let Some(id) = identity {
                b.set_certificate(id.cert()).unwrap();
                b.set_private_key(id.key()).unwrap();
            }
        })
    }

    #[tokio::test]
    async fn mqtt_acceptor_takes_a_client_ca_certificate() {
        let id = client_identity(&FIXTURE.client_ca, MQTT_UUID);
        let (client, server) = handshake_with(mqtt_client(Some(&id)), &mqtt_server()).await;
        client.expect("client handshake");
        let server = server.expect("server handshake");
        let peer = server.ssl().peer_certificate().expect("peer certificate");
        let cn = peer
            .subject_name()
            .entries_by_nid(Nid::COMMONNAME)
            .next()
            .unwrap();
        assert_eq!(cn.data().as_slice(), MQTT_UUID.as_bytes());
        assert_eq!(server.ssl().version_str(), "TLSv1.2");
    }

    async fn assert_mqtt_refused(identity: Option<&Identity>, what: &str) {
        let (client, server) = handshake_with(mqtt_client(identity), &mqtt_server()).await;
        assert!(client.is_err(), "{what}: client handshake must fail");
        assert!(
            matches!(server, Err(ControlError::Network { .. })),
            "{what}: server must refuse"
        );
    }

    #[tokio::test]
    async fn mqtt_acceptor_refuses_clients_without_a_client_ca_certificate() {
        assert_mqtt_refused(None, "no certificate").await;
        let wrong_ca = client_identity(&FIXTURE.ca, MQTT_UUID);
        assert_mqtt_refused(Some(&wrong_ca), "server CA certificate").await;
        // Right CA, right CN, but a serverAuth-only EKU.
        let server_only = FIXTURE.client_ca.issue_server(MQTT_UUID).unwrap();
        assert_mqtt_refused(Some(&server_only), "no clientAuth EKU").await;
    }

    // ---- mqtt_acceptor: foreign client certificates (D21) ----

    fn peer_cert(server: &SslStream<tokio::io::DuplexStream>) -> X509 {
        server.ssl().peer_certificate().expect("peer certificate")
    }

    #[tokio::test]
    async fn mqtt_acceptor_lets_a_foreign_client_certificate_through() {
        let foreign = client_identity(&FOREIGN_CA, MQTT_UUID);
        let (client, server) = handshake_with(mqtt_client(Some(&foreign)), &mqtt_server()).await;
        client.expect("client handshake");
        let server = server.expect("server handshake");
        let peer = peer_cert(&server);
        assert_eq!(
            peer.to_der().unwrap(),
            foreign.cert().to_der().unwrap(),
            "the broker sees the foreign certificate"
        );
        assert!(
            !chains_to_client_ca(
                &peer,
                server.ssl().peer_cert_chain(),
                FIXTURE.client_ca.cert()
            ),
            "a foreign certificate is not taken for one of ours"
        );
        let ours = our_ca_names(&[FIXTURE.client_ca.cert(), FIXTURE.ca.cert()]).unwrap();
        assert!(!issued_by_any(&peer, &ours));
    }

    #[tokio::test]
    async fn chains_to_client_ca_holds_for_our_client_certificates() {
        let id = client_identity(&FIXTURE.client_ca, MQTT_UUID);
        let (_client, server) = handshake_with(mqtt_client(Some(&id)), &mqtt_server()).await;
        let server = server.expect("server handshake");
        let peer = peer_cert(&server);
        assert!(chains_to_client_ca(
            &peer,
            server.ssl().peer_cert_chain(),
            FIXTURE.client_ca.cert()
        ));
        assert!(!chains_to_client_ca(&peer, None, FIXTURE.ca.cert()));
    }

    /// TLS still checks `CertificateVerify`: a client that presents a
    /// certificate without its private key fails, foreign issuer or not.
    #[tokio::test]
    async fn mqtt_acceptor_refuses_a_certificate_without_its_private_key() {
        let rsa = PKey::from_rsa(openssl::rsa::Rsa::generate(2048).unwrap()).unwrap();
        let pem = FOREIGN_CA
            .sign_csr(&csr_pem(&rsa, &[MQTT_UUID]), MQTT_UUID, 30)
            .unwrap();
        let cert = X509::from_pem(pem.as_bytes()).unwrap();
        let with_key = |key: &PKey<Private>| {
            client(|b| {
                b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
                b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
                b.set_cipher_list("ECDHE-RSA-AES256-SHA@SECLEVEL=0")
                    .unwrap();
                b.set_certificate(&cert).unwrap();
                b.set_private_key(key).unwrap();
                b.check_private_key().unwrap();
            })
        };
        // Control: the same certificate with its real key passes.
        let (client_ok, server_ok) = handshake_with(with_key(&rsa), &mqtt_server()).await;
        client_ok.expect("client handshake with the real key");
        server_ok.expect("server handshake with the real key");

        let wrong = mismatched_rsa_key(&rsa);
        let (client, server) = handshake_with(with_key(&wrong), &mqtt_server()).await;
        assert!(client.is_err(), "client without the private key must fail");
        let Err(ControlError::Network { message }) = server else {
            panic!("server must refuse a bad CertificateVerify");
        };
        assert!(message.contains("signature"), "{message}");
    }

    #[tokio::test]
    async fn mqtt_acceptor_refuses_a_foreign_certificate_with_the_wrong_purpose() {
        let server_only = FOREIGN_CA.issue_server(MQTT_UUID).unwrap();
        assert_mqtt_refused(Some(&server_only), "foreign, serverAuth only").await;
    }

    /// A foreign certificate past its notAfter still passes TLS (the pin is
    /// what counts, see `foreign_chain_error`); one of ours does not.
    #[tokio::test]
    async fn expired_certificates_pass_only_when_foreign() {
        let expired = |ca: &Ca| {
            let key = p256_key();
            let mut name = openssl::x509::X509NameBuilder::new().unwrap();
            name.append_entry_by_nid(Nid::COMMONNAME, MQTT_UUID)
                .unwrap();
            let name = name.build();
            let mut b = openssl::x509::X509Builder::new().unwrap();
            b.set_version(2).unwrap();
            b.set_subject_name(&name).unwrap();
            b.set_issuer_name(ca.cert().subject_name()).unwrap();
            b.set_pubkey(&key).unwrap();
            b.set_not_before(&openssl::asn1::Asn1Time::from_unix(1_500_000_000).unwrap())
                .unwrap();
            b.set_not_after(&openssl::asn1::Asn1Time::from_unix(1_600_000_000).unwrap())
                .unwrap();
            let eku = openssl::x509::extension::ExtendedKeyUsage::new()
                .client_auth()
                .build()
                .unwrap();
            b.append_extension(eku).unwrap();
            b.sign(ca.identity().key(), openssl::hash::MessageDigest::sha256())
                .unwrap();
            let pem = String::from_utf8(b.build().to_pem().unwrap()).unwrap();
            let key_pem =
                Secret::new(String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap());
            Identity::from_pem(&pem, &key_pem).unwrap()
        };
        let foreign = expired(&FOREIGN_CA);
        let (client, server) = handshake_with(mqtt_client(Some(&foreign)), &mqtt_server()).await;
        client.expect("client handshake");
        server.expect("an expired foreign certificate passes TLS");
        assert_mqtt_refused(Some(&expired(&FIXTURE.client_ca)), "expired, ours").await;
    }
}
