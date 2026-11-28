//! Local cloud HTTPS server (KB: AC — local control options, local cloud side).
//!
//! A clean-room stand-in for the three HTTPS endpoints the module calls
//! while it is being provisioned (rethink wiki, Thinq2CloudProtocol):
//!
//! - `GET /route` → `apiServer` / `mqttServer` URLs built from
//!   [`CloudConfig::advertised_host`].
//! - `GET /route/certificate` → the certificate names; with
//!   `?name=common-server|aws-iot` → our server CA as `certificatePem`.
//! - `POST /device/<uuid>/certificate` (PEM CSR body) → a client certificate
//!   signed by the client CA ([`Pki::client_ca`]), which is never returned
//!   by `/route/certificate`.
//!
//! Every reply is `{"resultCode":…,"result":…}`. Success is `"0000"`; on
//! errors we answer HTTP 400/403/404 with `resultCode` `"0400"`/`"0403"`/
//! `"0404"` and `result: null`. Those error codes are ours, not LG's.
//!
//! Hardening: the server only runs for one provisioning session
//! ([`CloudSession::start`] → [`CloudServer::shutdown`], aborted on drop),
//! and signs at most one CSR, only for the session's uuid. It listens on the
//! wildcard address of the configured address's family, because macOS only
//! lets unprivileged processes bind ports below 1024 there (a specific IP on
//! 443 gives EACCES); TCP peers outside the configured address's subnet
//! ([`IPV4_SUBNET_PREFIX`], [`IPV6_SUBNET_PREFIX`]) are dropped before the
//! TLS handshake. Connections are capped, and
//! handshakes, headers, bodies and whole connections are time-bounded. Logs
//! never contain the uuid or the CSR.

use std::convert::Infallible;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use openssl::ssl::SslAcceptor;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};

use super::pki::{Ca, MAX_CLIENT_DAYS, Pki};
use super::tls;
use crate::error::{ControlError, Result};

/// The HTTPS port advertised in `apiServer` (fixed: some modules ignore
/// non-default ports).
pub const API_PORT: u16 = 443;
/// The MQTTS port advertised in `mqttServer` (fixed, see [`API_PORT`]).
pub const MQTT_PORT: u16 = 8883;
/// Largest request body accepted (a CSR is about 1 KiB).
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// Most connections served at once; further ones are closed right away.
pub const MAX_CONNECTIONS: usize = 8;
/// Validity of the client certificate issued to the module.
pub const CLIENT_CERT_DAYS: u32 = MAX_CLIENT_DAYS;

/// Upper bound for receiving a request's headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for receiving a request's body.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for one connection, handshake included.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);
/// hyper's read buffer per connection (its minimum is 8 KiB).
const MAX_BUF_BYTES: usize = 16 * 1024;
/// Pause after a failed `accept` (for example out of file descriptors).
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

/// Prefix length of the IPv4 subnet whose peers may connect, derived from
/// [`CloudConfig::bind`]. std has no netmask API, so this assumes the home
/// LAN is a /24 (KB: Home network); a wider LAN would see its far hosts
/// dropped, a narrower one lets in a few extra neighbours of the same /24.
pub const IPV4_SUBNET_PREFIX: u32 = 24;
/// Prefix length of the IPv6 subnet whose peers may connect (a LAN is a /64).
pub const IPV6_SUBNET_PREFIX: u32 = 64;

/// The names `/route/certificate` lists; each returns our server CA.
const CERT_NAMES: [&str; 2] = ["common-server", "aws-iot"];

/// Where the server listens and what it tells the module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudConfig {
    /// This Mac's LAN address: the address the module is told about and the
    /// one the peer filter's subnet is derived from. The server itself
    /// listens on the unspecified address of the same family (see the module
    /// docs). The unspecified address (`0.0.0.0`, `::`) is refused here,
    /// since no subnet can be derived from it; loopback is allowed (tests).
    pub bind: IpAddr,
    /// The port to listen on: [`API_PORT`] in production, `0` in tests. The
    /// advertised ports stay [`API_PORT`] and [`MQTT_PORT`] either way.
    pub port: u16,
    /// The host put into `apiServer` and `mqttServer`: a DNS name, an IPv4
    /// address or an IPv6 address (written without brackets).
    pub advertised_host: String,
}

impl CloudConfig {
    /// Checks the bind address and the advertised host.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if `bind` is unspecified, or
    /// `advertised_host` is neither an IP address nor a hostname made of
    /// ASCII letters, digits, `-` and `.` (at most 253 characters).
    pub fn validate(&self) -> Result<()> {
        if self.bind.is_unspecified() {
            return Err(ControlError::invalid(
                "the local cloud must bind one interface address, not the unspecified address",
            ));
        }
        let host = self.advertised_host.as_str();
        if host.parse::<IpAddr>().is_ok() {
            return Ok(());
        }
        let hostname_ok = !host.is_empty()
            && host.len() <= 253
            && !host.starts_with(['.', '-'])
            && !host.contains("..")
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
        if hostname_ok {
            Ok(())
        } else {
            Err(ControlError::invalid(
                "the advertised host must be an IP address or a hostname",
            ))
        }
    }

    /// `https://<host>:443` (IPv6 hosts in brackets).
    #[must_use]
    pub fn api_server(&self) -> String {
        format!("https://{}:{API_PORT}", self.url_host())
    }

    /// `ssl://<host>:8883` (IPv6 hosts in brackets).
    #[must_use]
    pub fn mqtt_server(&self) -> String {
        format!("ssl://{}:{MQTT_PORT}", self.url_host())
    }

    fn url_host(&self) -> String {
        match self.advertised_host.parse::<IpAddr>() {
            Ok(IpAddr::V6(v6)) => format!("[{v6}]"),
            _ => self.advertised_host.clone(),
        }
    }
}

/// What the module has fetched so far in this session, for progress output.
/// Counts only successful replies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloudProgress {
    /// `GET /route` answers.
    pub route: u32,
    /// `GET /route/certificate` answers that carried our CA.
    pub ca: u32,
    /// Whether the session's one client certificate was issued.
    pub client_cert_issued: bool,
}

/// One provisioning session: the replies, the CA handed to the module, the
/// client CA and the one uuid whose CSR may be signed (once).
pub struct CloudSession {
    api_server: String,
    mqtt_server: String,
    server_ca_pem: String,
    client_ca: Ca,
    expected_uuid: String,
    signed: Mutex<bool>,
    progress: watch::Sender<CloudProgress>,
}

impl fmt::Debug for CloudSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudSession")
            .field("api_server", &self.api_server)
            .field("mqtt_server", &self.mqtt_server)
            .field("expected_uuid", &"***")
            .field("signed", &self.signed)
            .finish_non_exhaustive()
    }
}

/// A reply before it becomes an HTTP response.
#[derive(Debug, PartialEq)]
struct Reply {
    status: StatusCode,
    body: Value,
}

impl Reply {
    fn ok(result: Value) -> Self {
        Self {
            status: StatusCode::OK,
            body: Value::Object(serde_json::Map::from_iter([
                ("resultCode".to_owned(), Value::from("0000")),
                ("result".to_owned(), result),
            ])),
        }
    }

    /// Our own error shape: `resultCode` is the HTTP status, zero-padded.
    fn error(status: StatusCode) -> Self {
        Self {
            status,
            body: json!({ "resultCode": format!("{:04}", status.as_u16()), "result": null }),
        }
    }

    fn into_response(self) -> Response<Full<Bytes>> {
        let mut response = Response::new(Full::new(Bytes::from(self.body.to_string())));
        *response.status_mut() = self.status;
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        response
    }
}

impl CloudSession {
    /// Starts the HTTPS server for one provisioning session.
    ///
    /// `expected_uuid` is the module's uuid from `getDeviceInfo`: only a
    /// `POST /device/<expected_uuid>/certificate` with a CSR whose common
    /// name is that uuid gets signed, and only once.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if the config or uuid is invalid or the TLS
    /// profile cannot be built; [`ControlError::Network`] if binding fails
    /// (the message names the port, never an IP).
    pub async fn start(
        config: &CloudConfig,
        pki: &Pki,
        expected_uuid: &str,
    ) -> Result<CloudServer> {
        let session = Arc::new(Self::new(config, pki, expected_uuid)?);
        warn_on_san_mismatch(config, pki);
        let acceptor = Arc::new(tls::server_acceptor(pki.server.cert(), pki.server.key())?);
        let listener = TcpListener::bind(SocketAddr::new(wildcard_of(config.bind), config.port))
            .await
            .map_err(|e| bind_error(config.port, &e))?;
        let listen_addr = listener
            .local_addr()
            .map_err(|e| bind_error(config.port, &e))?;
        tracing::info!(
            port = listen_addr.port(),
            "cloud: provisioning session started"
        );
        let progress = session.progress.subscribe();
        let task = tokio::spawn(accept_loop(listener, acceptor, session, config.bind));
        Ok(CloudServer {
            local_addr: SocketAddr::new(config.bind, listen_addr.port()),
            listen_addr,
            task: Some(task),
            progress,
        })
    }

    fn new(config: &CloudConfig, pki: &Pki, expected_uuid: &str) -> Result<Self> {
        config.validate()?;
        if expected_uuid.is_empty() || expected_uuid.contains(['/', '?', '#']) {
            return Err(ControlError::invalid("device uuid is empty or malformed"));
        }
        if expected_uuid.chars().any(char::is_whitespace) {
            return Err(ControlError::invalid("device uuid is empty or malformed"));
        }
        Ok(Self {
            api_server: config.api_server(),
            mqtt_server: config.mqtt_server(),
            server_ca_pem: pki.ca.identity().cert_pem()?,
            client_ca: pki.client_ca.clone(),
            expected_uuid: expected_uuid.to_owned(),
            signed: Mutex::new(false),
            progress: watch::Sender::new(CloudProgress::default()),
        })
    }

    /// Reads the body (bounded in size and time), then answers.
    async fn serve(&self, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let (parts, body) = request.into_parts();
        let body = tokio::time::timeout(
            BODY_READ_TIMEOUT,
            Limited::new(body, MAX_BODY_BYTES).collect(),
        )
        .await;
        let reply = match body {
            Ok(Ok(collected)) => {
                let bytes = collected.to_bytes();
                self.respond(&parts.method, parts.uri.path(), parts.uri.query(), &bytes)
            }
            Ok(Err(_)) => self.refuse(
                StatusCode::BAD_REQUEST,
                "body too large or unreadable",
                &parts.method,
                parts.uri.path(),
                None,
            ),
            Err(_) => self.refuse(
                StatusCode::BAD_REQUEST,
                "body read timed out",
                &parts.method,
                parts.uri.path(),
                None,
            ),
        };
        reply.into_response()
    }

    /// Routes one request with its full body.
    fn respond(&self, method: &Method, path: &str, query: Option<&str>, body: &[u8]) -> Reply {
        let reply = match (method, path, device_uuid(path)) {
            (&Method::POST, _, Some(uuid)) => self.sign(method, path, uuid, body),
            (&Method::GET, "/route", _) => {
                self.progress.send_modify(|p| p.route += 1);
                Reply::ok(json!({
                    "apiServer": self.api_server,
                    "mqttServer": self.mqtt_server,
                }))
            }
            (&Method::GET, "/route/certificate", _) => match query_param(query, "name") {
                None => Reply::ok(json!(CERT_NAMES)),
                Some(name) if CERT_NAMES.contains(&name) => {
                    self.progress.send_modify(|p| p.ca += 1);
                    Reply::ok(json!({ "certificatePem": self.server_ca_pem }))
                }
                Some(_) => Reply::error(StatusCode::NOT_FOUND),
            },
            _ => Reply::error(StatusCode::NOT_FOUND),
        };
        tracing::info!(
            %method,
            path = redacted_path(path),
            status = reply.status.as_u16(),
            "cloud: request"
        );
        reply
    }

    /// `POST /device/<uuid>/certificate`: signs only for the session's uuid,
    /// only a CSR with that CN, at most once per session.
    fn sign(&self, method: &Method, path: &str, uuid: &str, body: &[u8]) -> Reply {
        let refuse = |status, reason: &str| self.refuse(status, reason, method, path, Some(body));
        if uuid != self.expected_uuid {
            return refuse(StatusCode::FORBIDDEN, "uuid is not this session's device");
        }
        let Ok(csr) = std::str::from_utf8(body) else {
            return refuse(StatusCode::BAD_REQUEST, "body is not UTF-8");
        };
        let mut signed = self.signed.lock().unwrap_or_else(PoisonError::into_inner);
        if *signed {
            return refuse(
                StatusCode::FORBIDDEN,
                "a certificate was already signed in this session",
            );
        }
        match self
            .client_ca
            .sign_csr(csr, &self.expected_uuid, CLIENT_CERT_DAYS)
        {
            Ok(pem) => {
                *signed = true;
                self.progress.send_modify(|p| p.client_cert_issued = true);
                tracing::info!("cloud: client certificate issued");
                Reply::ok(json!({ "certificatePem": pem }))
            }
            Err(e) => refuse(StatusCode::BAD_REQUEST, &e.to_string()),
        }
    }

    /// Logs a refused request (reason, method, redacted path, body length;
    /// never the body) and builds the error reply.
    #[allow(clippy::unused_self)] // keeps the call sites uniform
    fn refuse(
        &self,
        status: StatusCode,
        reason: &str,
        method: &Method,
        path: &str,
        body: Option<&[u8]>,
    ) -> Reply {
        tracing::warn!(
            reason,
            %method,
            path = redacted_path(path),
            body_len = body.map(<[u8]>::len),
            status = status.as_u16(),
            "cloud: request refused"
        );
        Reply::error(status)
    }
}

/// The `<uuid>` of `/device/<uuid>/certificate`, if `path` has that shape.
/// The unspecified address of `ip`'s family (`0.0.0.0` or `::`).
fn wildcard_of(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

/// Maps a bind failure to an error naming the port only (the address is
/// always the wildcard; the Mac's IP stays out of messages).
fn bind_error(port: u16, e: &io::Error) -> ControlError {
    if e.kind() == io::ErrorKind::PermissionDenied {
        ControlError::network(format!(
            "local cloud bind: permission denied for port {port} on the wildcard address \
             (macOS lets unprivileged processes bind ports below 1024 only on the \
             wildcard address): {e}"
        ))
    } else {
        ControlError::network(format!("local cloud bind on port {port}: {e}"))
    }
}

/// Whether `peer` is in the same subnet as `bind` (/[`IPV4_SUBNET_PREFIX`] or
/// /[`IPV6_SUBNET_PREFIX`]). IPv4-mapped IPv6 addresses count as IPv4, so a
/// dual-stack `::` listener cannot be reached across families.
pub(crate) fn same_subnet(bind: IpAddr, peer: IpAddr) -> bool {
    match (bind.to_canonical(), peer.to_canonical()) {
        (IpAddr::V4(b), IpAddr::V4(p)) => {
            let mask = u32::MAX << (32 - IPV4_SUBNET_PREFIX);
            u32::from(b) & mask == u32::from(p) & mask
        }
        (IpAddr::V6(b), IpAddr::V6(p)) => {
            let mask = u128::MAX << (128 - IPV6_SUBNET_PREFIX);
            u128::from(b) & mask == u128::from(p) & mask
        }
        _ => false,
    }
}

/// Warns when the server certificate has no SAN for the advertised host.
///
/// Not an error (the PKI is normally loaded for the same host, which
/// re-issues the certificate), but without the warning the module would fail
/// only after `/route` succeeds, with no visible reason.
fn warn_on_san_mismatch(config: &CloudConfig, pki: &Pki) {
    if !pki.server.has_san(&config.advertised_host) {
        tracing::warn!(
            host = %config.advertised_host,
            "cloud: server certificate has no SAN for the advertised host; \
             the module will likely reject the TLS handshake after /route"
        );
    }
}

fn device_uuid(path: &str) -> Option<&str> {
    let uuid = path
        .strip_prefix("/device/")?
        .strip_suffix("/certificate")?;
    (!uuid.is_empty() && !uuid.contains('/')).then_some(uuid)
}

/// The path for logs: anything under `/device/` hides the uuid.
fn redacted_path(path: &str) -> &str {
    if path.starts_with("/device/") {
        "/device/***"
    } else {
        path
    }
}

/// The first value of `key` in a `a=b&c=d` query (not percent-decoded: the
/// names we accept are plain ASCII).
fn query_param<'q>(query: Option<&'q str>, key: &str) -> Option<&'q str> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(k, v)| (k == key).then_some(v))
}

/// The running server of one provisioning session. Stop it with
/// [`shutdown`](Self::shutdown); dropping it also stops it.
#[derive(Debug)]
pub struct CloudServer {
    local_addr: SocketAddr,
    listen_addr: SocketAddr,
    task: Option<JoinHandle<()>>,
    progress: watch::Receiver<CloudProgress>,
}

impl CloudServer {
    /// Where the module reaches the server: [`CloudConfig::bind`] with the
    /// port actually bound (useful with port 0).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The socket actually listened on: the wildcard address of
    /// [`CloudConfig::bind`]'s family and the bound port.
    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// What the module has fetched so far; changes as requests are answered.
    #[must_use]
    pub fn progress(&self) -> watch::Receiver<CloudProgress> {
        self.progress.clone()
    }

    /// Stops accepting, closes every open connection and waits until the
    /// listener is closed.
    pub async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            // A cancelled task is the expected outcome.
            let _ = task.await;
        }
        tracing::info!("cloud: provisioning session stopped");
    }
}

impl Drop for CloudServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Accepts connections until aborted; aborting drops the `JoinSet`, which
/// aborts every connection task. Peers outside `bind`'s subnet are closed
/// before the TLS handshake.
async fn accept_loop(
    listener: TcpListener,
    acceptor: Arc<SslAcceptor>,
    session: Arc<CloudSession>,
    bind: IpAddr,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let stream = match listener.accept().await {
            Ok((stream, peer)) if same_subnet(bind, peer.ip()) => stream,
            Ok(_) => {
                // No address: the module's and the Mac's IPs stay out of logs.
                tracing::debug!("cloud: dropped a connection from outside the LAN subnet");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cloud: accept failed");
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            tracing::warn!(
                max = MAX_CONNECTIONS,
                "cloud: too many connections; closing one"
            );
            continue;
        };
        connections.spawn(serve_connection(
            stream,
            Arc::clone(&acceptor),
            Arc::clone(&session),
            permit,
        ));
    }
}

async fn serve_connection(
    stream: TcpStream,
    acceptor: Arc<SslAcceptor>,
    session: Arc<CloudSession>,
    _permit: OwnedSemaphorePermit,
) {
    let connection = async {
        let tls = match tls::accept(&acceptor, stream).await {
            Ok(tls) => tls,
            Err(e) => {
                tracing::debug!(error = %e, "cloud: TLS handshake failed");
                return;
            }
        };
        let service = service_fn(move |request| {
            let session = Arc::clone(&session);
            async move { Ok::<_, Infallible>(session.serve(request).await) }
        });
        let served = http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .max_buf_size(MAX_BUF_BYTES)
            .serve_connection(TokioIo::new(tls), service)
            .await;
        if let Err(e) = served {
            tracing::debug!(error = %e, "cloud: connection ended with an error");
        }
    };
    if tokio::time::timeout(CONNECTION_TIMEOUT, connection)
        .await
        .is_err()
    {
        tracing::debug!("cloud: connection timed out");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ac::pki::test_fixture::{FIXTURE, HOSTNAME, chains_to, csr_pem, p256_key};
    use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};
    use openssl::x509::X509;
    use std::pin::Pin;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_openssl::SslStream;

    const UUID: &str = "00000000-fake-0000-0000-000000000000";

    fn config(host: &str) -> CloudConfig {
        CloudConfig {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            advertised_host: host.to_owned(),
        }
    }

    fn session() -> CloudSession {
        CloudSession::new(&config(HOSTNAME), &FIXTURE, UUID).unwrap()
    }

    fn csr() -> String {
        csr_pem(&p256_key(), &[UUID])
    }

    fn post_path(uuid: &str) -> String {
        format!("/device/{uuid}/certificate")
    }

    /// Replaces every PEM string by `<pem>` (keys and serials change per run).
    fn redact(value: &mut Value) {
        match value {
            Value::String(s) if s.starts_with("-----BEGIN ") => *s = "<pem>".to_owned(),
            Value::Array(items) => items.iter_mut().for_each(redact),
            Value::Object(map) => map.values_mut().for_each(redact),
            _ => {}
        }
    }

    fn snapshot_text(reply: &Reply) -> String {
        let mut body = reply.body.clone();
        redact(&mut body);
        format!(
            "HTTP {}\n{}",
            reply.status.as_u16(),
            serde_json::to_string_pretty(&body).unwrap()
        )
    }

    fn get(session: &CloudSession, path: &str, query: Option<&str>) -> Reply {
        session.respond(&Method::GET, path, query, b"")
    }

    fn post(session: &CloudSession, uuid: &str, body: &str) -> Reply {
        session.respond(&Method::POST, &post_path(uuid), None, body.as_bytes())
    }

    #[test]
    fn route_snapshot() {
        insta::assert_snapshot!("route", snapshot_text(&get(&session(), "/route", None)));
    }

    #[test]
    fn certificate_snapshots() {
        let s = session();
        insta::assert_snapshot!(
            "certificate_list",
            snapshot_text(&get(&s, "/route/certificate", None))
        );
        for name in CERT_NAMES {
            let reply = get(&s, "/route/certificate", Some(&format!("name={name}")));
            assert_eq!(
                reply.body["result"]["certificatePem"],
                FIXTURE.ca.identity().cert_pem().unwrap(),
                "{name} must be the server CA"
            );
            insta::assert_snapshot!(format!("certificate_{name}"), snapshot_text(&reply));
        }
        insta::assert_snapshot!(
            "certificate_unknown_name",
            snapshot_text(&get(&s, "/route/certificate", Some("name=other")))
        );
    }

    #[test]
    fn not_found_snapshots() {
        let s = session();
        insta::assert_snapshot!("unknown_path", snapshot_text(&get(&s, "/other", None)));
        insta::assert_snapshot!(
            "route_wrong_method",
            snapshot_text(&s.respond(&Method::POST, "/route", None, b""))
        );
        insta::assert_snapshot!(
            "device_certificate_wrong_method",
            snapshot_text(&get(&s, &post_path(UUID), None))
        );
    }

    #[test]
    fn csr_signing_snapshots() {
        let s = session();
        insta::assert_snapshot!(
            "csr_wrong_uuid",
            snapshot_text(&post(&s, "11111111-fake-1111-1111-111111111111", &csr()))
        );
        insta::assert_snapshot!(
            "csr_cn_mismatch",
            snapshot_text(&post(
                &s,
                UUID,
                &csr_pem(&p256_key(), &["11111111-fake-1111-1111-111111111111"])
            ))
        );
        insta::assert_snapshot!(
            "csr_not_utf8",
            snapshot_text(&s.respond(&Method::POST, &post_path(UUID), None, &[0xff, 0xfe]))
        );
        insta::assert_snapshot!("csr_signed", snapshot_text(&post(&s, UUID, &csr())));
        insta::assert_snapshot!("csr_second", snapshot_text(&post(&s, UUID, &csr())));
    }

    #[test]
    fn csr_signing_is_tied_to_the_session() {
        let s = session();
        let other = "11111111-fake-1111-1111-111111111111";
        assert_eq!(post(&s, other, &csr()).status, StatusCode::FORBIDDEN);
        assert_eq!(
            post(&s, UUID, &csr_pem(&p256_key(), &[other])).status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(post(&s, UUID, "garbage").status, StatusCode::BAD_REQUEST);
        // Failed attempts do not use up the session's one signature.
        let first = post(&s, UUID, &csr());
        assert_eq!(first.status, StatusCode::OK);
        let pem = first.body["result"]["certificatePem"].as_str().unwrap();
        let cert = X509::from_pem(pem.as_bytes()).unwrap();
        assert!(chains_to(&cert, FIXTURE.client_ca.cert()));
        assert_eq!(post(&s, UUID, &csr()).status, StatusCode::FORBIDDEN);
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn logs_hide_the_uuid_and_csr() {
        let s = session();
        let csr = csr();
        let body_line = csr.lines().nth(2).unwrap().to_owned();
        let other = "11111111-fake-1111-1111-111111111111";
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            post(&s, other, &csr);
            post(&s, UUID, &csr_pem(&p256_key(), &[other]));
            post(&s, UUID, &csr);
            post(&s, UUID, &csr);
            tracing::info!(session = ?s, "debug output");
        });
        let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("uuid is not this session's device"), "{logs}");
        assert!(logs.contains("already signed"), "{logs}");
        assert!(logs.contains(&format!("body_len={}", csr.len())), "{logs}");
        assert!(logs.contains("/device/***"), "{logs}");
        for secret in [UUID, other, "CERTIFICATE REQUEST", body_line.as_str()] {
            assert!(!logs.contains(secret), "leaked {secret}: {logs}");
        }
    }

    fn san_warning_logs(host: &str) -> String {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            warn_on_san_mismatch(&config(host), &FIXTURE);
        });
        String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn san_mismatch_is_warned_about() {
        assert_eq!(san_warning_logs(HOSTNAME), "");
        let ip_pki = Pki {
            server: FIXTURE.ca.issue_server("192.0.2.10").unwrap(),
            ..FIXTURE.clone()
        };
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            warn_on_san_mismatch(&config("192.0.2.10"), &ip_pki);
        });
        assert!(
            buffer.0.lock().unwrap().is_empty(),
            "an IP SAN matches an IP host"
        );
        for host in ["192.0.2.10", "other.test"] {
            let logs = san_warning_logs(host);
            assert!(logs.contains("WARN"), "{logs}");
            assert!(logs.contains("no SAN for the advertised host"), "{logs}");
            assert!(logs.contains(&format!("host={host}")), "{logs}");
        }
    }

    #[test]
    fn advertised_host_accepts_ips_and_names() {
        let c = config("192.0.2.10");
        c.validate().unwrap();
        assert_eq!(c.api_server(), "https://192.0.2.10:443");
        assert_eq!(c.mqtt_server(), "ssl://192.0.2.10:8883");
        let c = config("2001:db8::1");
        c.validate().unwrap();
        assert_eq!(c.api_server(), "https://[2001:db8::1]:443");
        assert_eq!(c.mqtt_server(), "ssl://[2001:db8::1]:8883");
        let c = config("controlz.test");
        c.validate().unwrap();
        assert_eq!(c.api_server(), "https://controlz.test:443");
        assert_eq!(c.mqtt_server(), "ssl://controlz.test:8883");
        // The route reply uses the configured host, nothing else.
        let s = CloudSession::new(&config("192.0.2.10"), &FIXTURE, UUID).unwrap();
        assert_eq!(
            get(&s, "/route", None).body,
            json!({"resultCode": "0000", "result": {
                "apiServer": "https://192.0.2.10:443",
                "mqttServer": "ssl://192.0.2.10:8883",
            }})
        );
    }

    #[test]
    fn bad_config_is_refused() {
        for host in [
            "",
            " ",
            "a b",
            "host/path",
            "user@host",
            "host:443",
            "[::1]",
            ".x",
            "a..b",
        ] {
            assert!(config(host).validate().is_err(), "{host:?} must be refused");
        }
        for bind in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            let c = CloudConfig {
                bind,
                ..config(HOSTNAME)
            };
            assert!(c.validate().is_err(), "{bind} must be refused");
        }
        for uuid in ["", "a/b", "a b"] {
            assert!(CloudSession::new(&config(HOSTNAME), &FIXTURE, uuid).is_err());
        }
    }

    #[tokio::test]
    async fn start_refuses_the_unspecified_address() {
        let c = CloudConfig {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ..config(HOSTNAME)
        };
        let result = CloudSession::start(&c, &FIXTURE, UUID).await;
        assert!(matches!(result, Err(ControlError::Invalid { .. })));
    }

    #[test]
    fn peer_filter_keeps_the_bind_subnet() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let v4 = ip("192.0.2.10");
        for peer in [
            "192.0.2.10",
            "192.0.2.1",
            "192.0.2.255",
            "::ffff:192.0.2.77",
        ] {
            assert!(same_subnet(v4, ip(peer)), "{peer} must be accepted");
        }
        for peer in [
            "192.0.3.10",
            "198.51.100.10",
            "127.0.0.1",
            "0.0.0.0",
            "2001:db8::10",
            "::ffff:198.51.100.10",
        ] {
            assert!(!same_subnet(v4, ip(peer)), "{peer} must be dropped");
        }
        let v6 = ip("2001:db8:0:1::10");
        for peer in ["2001:db8:0:1::1", "2001:db8:0:1:ffff:ffff:ffff:ffff"] {
            assert!(same_subnet(v6, ip(peer)), "{peer} must be accepted");
        }
        for peer in ["2001:db8:0:2::10", "2001:db8:1:1::10", "::1", "192.0.2.10"] {
            assert!(!same_subnet(v6, ip(peer)), "{peer} must be dropped");
        }
        // Loopback binds (tests) accept loopback peers.
        assert!(same_subnet(ip("127.0.0.1"), ip("127.0.0.1")));
        assert!(same_subnet(ip("::1"), ip("::1")));
    }

    #[test]
    fn bind_errors_name_the_port_and_the_macos_rule() {
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let message = bind_error(443, &denied).to_string();
        assert!(message.contains("port 443"), "{message}");
        assert!(
            message.contains("only on the wildcard address"),
            "{message}"
        );
        let in_use = io::Error::from(io::ErrorKind::AddrInUse);
        let message = bind_error(443, &in_use).to_string();
        assert!(
            message.contains("local cloud bind on port 443"),
            "{message}"
        );
        assert!(!message.contains("wildcard"), "{message}");
    }

    #[test]
    fn wildcard_keeps_the_family() {
        assert_eq!(
            wildcard_of(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            wildcard_of("2001:db8::10".parse().unwrap()),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        );
    }

    // ---- Mock device over real TCP ----

    /// A client like the module: TLS 1.2, `ECDHE-RSA-AES256-SHA` only.
    /// Without `trust` it accepts any certificate (as the module does for
    /// `/route`); with it, it verifies the chain and the SAN.
    fn module_client(trust: Option<&X509>) -> SslConnector {
        let mut b = SslConnector::builder(SslMethod::tls_client()).unwrap();
        b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        b.set_cipher_list("ECDHE-RSA-AES256-SHA@SECLEVEL=0")
            .unwrap();
        match trust {
            Some(ca) => b.cert_store_mut().add_cert(ca.clone()).unwrap(),
            None => b.set_verify(SslVerifyMode::NONE),
        }
        b.build()
    }

    /// One HTTP/1.1 request on a fresh TLS connection; returns (status, body).
    async fn request(
        addr: SocketAddr,
        trust: Option<&X509>,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        let tcp = TcpStream::connect(addr).await.unwrap();
        let ssl = module_client(trust)
            .configure()
            .unwrap()
            .into_ssl(HOSTNAME)
            .unwrap();
        let mut tls = SslStream::new(ssl, tcp).unwrap();
        Pin::new(&mut tls).connect().await.expect("TLS handshake");
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {HOSTNAME}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tls.write_all(head.as_bytes()).await.unwrap();
        tls.write_all(body).await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0_u8; 4096];
        loop {
            match tls.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
            if let Some(response) = parse_response(&raw) {
                return response;
            }
        }
        parse_response(&raw).expect("complete HTTP response")
    }

    /// Parses a full response with a `content-length`, or `None` if incomplete.
    fn parse_response(raw: &[u8]) -> Option<(u16, Vec<u8>)> {
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = std::str::from_utf8(&raw[..end]).unwrap();
        let status = head.split(' ').nth(1)?.parse().ok()?;
        let length: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        let body = raw.get(end + 4..end + 4 + length)?;
        Some((status, body.to_vec()))
    }

    fn json_body(body: &[u8]) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    async fn start() -> CloudServer {
        CloudSession::start(&config(HOSTNAME), &FIXTURE, UUID)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn mock_device_provisions_over_tls() {
        let server = start().await;
        let addr = server.local_addr();
        // Listens on the wildcard, reports and advertises the configured host.
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(server.listen_addr().ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(server.listen_addr().port(), addr.port());

        // 1. Route, accepting any certificate.
        let (status, body) = request(addr, None, "GET", "/route", b"").await;
        assert_eq!(status, 200);
        assert_eq!(
            json_body(&body)["result"],
            json!({"apiServer": "https://controlz.test:443", "mqttServer": "ssl://controlz.test:8883"})
        );

        // 2. Certificate names, then each certificate.
        let (status, body) = request(addr, None, "GET", "/route/certificate", b"").await;
        assert_eq!(status, 200);
        assert_eq!(json_body(&body)["result"], json!(CERT_NAMES));
        let mut trusted = Vec::new();
        for name in CERT_NAMES {
            let path = format!("/route/certificate?name={name}");
            let (status, body) = request(addr, None, "GET", &path, b"").await;
            assert_eq!(status, 200);
            let pem = json_body(&body)["result"]["certificatePem"]
                .as_str()
                .unwrap()
                .to_owned();
            trusted.push(X509::from_pem(pem.as_bytes()).unwrap());
        }
        let returned_ca = &trusted[0];
        assert_eq!(
            returned_ca.to_der().unwrap(),
            trusted[1].to_der().unwrap(),
            "both names return the same CA"
        );
        assert_eq!(
            returned_ca.to_der().unwrap(),
            FIXTURE.ca.cert().to_der().unwrap()
        );
        assert!(chains_to(FIXTURE.server.cert(), returned_ca));

        // 3. CSR, now verifying the server against the returned CA (chain + SAN).
        let key = p256_key();
        let csr = csr_pem(&key, &[UUID]);
        let (status, body) = request(
            addr,
            Some(returned_ca),
            "POST",
            &post_path(UUID),
            csr.as_bytes(),
        )
        .await;
        assert_eq!(status, 200);
        let pem = json_body(&body)["result"]["certificatePem"]
            .as_str()
            .unwrap()
            .to_owned();
        let cert = X509::from_pem(pem.as_bytes()).unwrap();
        assert!(cert.public_key().unwrap().public_eq(&key));
        assert!(
            chains_to(&cert, FIXTURE.client_ca.cert()),
            "issued by our client CA"
        );
        assert!(
            !chains_to(&cert, returned_ca),
            "must not chain to the CA the module trusts for servers"
        );

        // 4. A second CSR in the same session is refused.
        let (status, body) = request(
            addr,
            Some(returned_ca),
            "POST",
            &post_path(UUID),
            csr.as_bytes(),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(
            json_body(&body),
            json!({"resultCode": "0403", "result": null})
        );
        // Progress counts the successful replies only: one route, two CAs
        // (the name list is not a CA), one certificate.
        assert_eq!(
            *server.progress().borrow(),
            CloudProgress {
                route: 1,
                ca: 2,
                client_cert_issued: true
            }
        );

        server.shutdown().await;
        assert!(TcpStream::connect(addr).await.is_err(), "listener closed");
    }

    #[tokio::test]
    async fn oversized_body_is_refused() {
        let server = start().await;
        let body = vec![b'a'; MAX_BODY_BYTES + 1];
        let (status, _) = request(server.local_addr(), None, "POST", &post_path(UUID), &body).await;
        assert_eq!(status, 400);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn connections_are_capped() {
        let server = start().await;
        let addr = server.local_addr();
        let mut idle = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            idle.push(TcpStream::connect(addr).await.unwrap());
        }
        // Give the accept loop time to take all permits.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut extra = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(2), extra.read(&mut buf))
            .await
            .expect("the extra connection must be closed, not left waiting");
        assert!(matches!(read, Ok(0) | Err(_)), "extra connection closed");
        drop(idle);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn dropping_the_server_stops_it() {
        let server = start().await;
        let addr = server.local_addr();
        drop(server);
        for _ in 0..100 {
            if TcpStream::connect(addr).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("listener still open after drop");
    }
}
