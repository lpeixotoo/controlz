//! Provisioning DNS fallback (KB: AC — local control options, DNS plan).
//!
//! The module ignores the `apiServer` / `mqttServer` we pass in
//! `getDeviceInfo` and first asks `common.lgthinq.com` (firmware also has the
//! typo `comon.lgthinq.com`) for `/route` (rethink wiki pages
//! `Installing-rethink-cloud` and `Thinq2CloudProtocol`). While the router hands out this Mac as the
//! LAN's DNS server, this small resolver answers the overridden names with
//! [`DnsConfig::bind`] and relays every other query, unchanged, to
//! [`DnsConfig::upstream`].
//!
//! Answers: an overridden name gets one `A` record (or `AAAA` when `bind` is
//! IPv6) with a non-zero TTL ([`ANSWER_TTL`]; the firmware mishandles TTL 0)
//! and the AA flag; any other type for an overridden name gets an empty
//! NOERROR answer. The question is copied byte for byte, so `0x20` case
//! randomisation survives. Messages that are not a standard query with
//! exactly one question are dropped.
//!
//! Forwarding: one fresh connected UDP socket per query (random source port,
//! replies must carry the query id), [`UPSTREAM_TIMEOUT`] per attempt, one
//! retry, then one TCP try, then SERVFAIL. Replies with TC set are passed
//! through so the client retries over TCP; queries that arrive over TCP are
//! forwarded over TCP.
//!
//! Hardening matches the local cloud: it listens on the wildcard address of
//! `bind`'s family (macOS lets unprivileged processes bind ports below 1024
//! only there), drops peers outside `bind`'s subnet, caps queries in flight
//! and TCP connections, bounds every wait and stops on
//! [`DnsServer::shutdown`] or drop. It is meant to run only while the module
//! is being provisioned. Logs: the first sighting of each (peer, name, type)
//! is logged at info when the name is overridden or mentions `lgthinq` or
//! `amazonaws`, at debug otherwise.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hickory_proto::op::{Message, MessageType, OpCode};
use hickory_proto::rr::{DNSClass, RecordType};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};

use super::cloud::same_subnet;
use crate::error::{ControlError, Result};

/// The DNS port, for UDP and TCP.
pub const DNS_PORT: u16 = 53;
/// Where other names are forwarded unless configured otherwise.
pub const DEFAULT_UPSTREAM: IpAddr = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
/// TTL of our answers: short, and never 0 (the firmware mishandles TTL 0).
pub const ANSWER_TTL: u32 = 60;
/// Queries being forwarded at once; further UDP queries are dropped (the
/// client retries) and further TCP queries get SERVFAIL.
pub const MAX_IN_FLIGHT: usize = 256;
/// TCP connections served at once; further ones are closed right away.
pub const MAX_TCP_CONNECTIONS: usize = 64;
/// Upper bound for one upstream attempt (UDP exchange or whole TCP exchange).
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);

/// A TCP connection is closed after this long without a complete query.
const TCP_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest UDP message accepted or relayed.
const MAX_UDP_BYTES: usize = 4096;
/// Distinct (peer, name, type) triples remembered for the first-sighting log.
const MAX_LOGGED: usize = 4096;
/// Attempts to find a port free for both UDP and TCP when `port` is 0.
const EPHEMERAL_BIND_ATTEMPTS: usize = 8;
/// Pause after a failed `recv_from` / `accept`.
const ERROR_PAUSE: Duration = Duration::from_millis(100);
/// Length of the fixed DNS header.
const HEADER_LEN: usize = 12;
/// Response codes we produce.
const RCODE_NOERROR: u8 = 0;
const RCODE_SERVFAIL: u8 = 2;

/// A name the resolver answers itself.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NameRule {
    /// Exactly this name.
    Exact(String),
    /// Every name below this domain (not the domain itself), like `*.domain`.
    Suffix(String),
}

impl NameRule {
    /// Parses `name` (an exact name) or `.domain` (every name below it).
    /// Case-insensitive; a trailing dot is ignored.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] unless the name is 1–253 characters of
    /// labels made of ASCII letters, digits, `-` and `_`.
    pub fn parse(rule: &str) -> Result<Self> {
        let (suffix, name) = match rule.strip_prefix('.') {
            Some(rest) => (true, rest),
            None => (false, rule),
        };
        let name = normalize(name);
        let label_ok = |l: &str| {
            (1..=63).contains(&l.len())
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        if name.is_empty() || name.len() > 253 || !name.split('.').all(label_ok) {
            return Err(ControlError::invalid(format!(
                "DNS name {rule:?} is not a name (or .domain for every name below it)"
            )));
        }
        Ok(if suffix {
            Self::Suffix(name)
        } else {
            Self::Exact(name)
        })
    }

    /// Whether `name` (normalized: lowercase, no trailing dot) matches.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::Exact(exact) => name == exact,
            Self::Suffix(domain) => name
                .strip_suffix(domain.as_str())
                .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
        }
    }
}

impl fmt::Display for NameRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(name) => f.write_str(name),
            Self::Suffix(domain) => write!(f, ".{domain}"),
        }
    }
}

/// The names answered by default: LG's setup host (and its firmware typo)
/// and everything under `lgthinq.com`.
#[must_use]
pub fn default_overrides() -> Vec<NameRule> {
    vec![
        NameRule::Exact("common.lgthinq.com".to_owned()),
        NameRule::Exact("comon.lgthinq.com".to_owned()),
        NameRule::Suffix("lgthinq.com".to_owned()),
    ]
}

/// Lowercase, without a trailing dot.
fn normalize(name: &str) -> String {
    name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase()
}

/// Where the resolver listens, what it answers and where it forwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsConfig {
    /// This Mac's LAN address: the address overridden names resolve to and
    /// the one the peer filter's subnet is derived from. The resolver listens
    /// on the unspecified address of the same family.
    pub bind: IpAddr,
    /// The port to listen on (UDP and TCP): [`DNS_PORT`], or `0` in tests.
    pub port: u16,
    /// The resolver other queries are relayed to.
    pub upstream: SocketAddr,
    /// Names answered with `bind`.
    pub overrides: Vec<NameRule>,
    /// TTL of those answers, in seconds (non-zero).
    pub answer_ttl: u32,
}

impl DnsConfig {
    /// Port 53, the default names, [`ANSWER_TTL`] and `upstream` on port 53.
    #[must_use]
    pub fn new(bind: IpAddr, upstream: IpAddr) -> Self {
        Self {
            bind,
            port: DNS_PORT,
            upstream: SocketAddr::new(upstream, DNS_PORT),
            overrides: default_overrides(),
            answer_ttl: ANSWER_TTL,
        }
    }

    /// Checks the addresses and the TTL.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if `bind` or `upstream` is unspecified, the
    /// upstream is this resolver itself, or the TTL is 0.
    pub fn validate(&self) -> Result<()> {
        if self.bind.is_unspecified() {
            return Err(ControlError::invalid(
                "the local DNS must answer with one interface address, not the unspecified address",
            ));
        }
        if self.upstream.ip().is_unspecified() || self.upstream.port() == 0 {
            return Err(ControlError::invalid(
                "the DNS upstream needs an address and a port",
            ));
        }
        if self.upstream.ip().to_canonical() == self.bind.to_canonical()
            && self.upstream.port() == self.port
        {
            return Err(ControlError::invalid(
                "the DNS upstream is this resolver itself (a forwarding loop)",
            ));
        }
        if self.answer_ttl == 0 {
            return Err(ControlError::invalid(
                "the DNS answer TTL must not be 0 (the module mishandles it)",
            ));
        }
        Ok(())
    }

    fn is_overridden(&self, name: &str) -> bool {
        self.overrides.iter().any(|rule| rule.matches(name))
    }
}

/// The running resolver. Stop it with [`shutdown`](Self::shutdown); dropping
/// it also stops it.
#[derive(Debug)]
pub struct DnsServer {
    listen_addr: SocketAddr,
    tasks: Vec<JoinHandle<()>>,
}

impl DnsServer {
    /// Binds UDP and TCP on the wildcard address of `config.bind`'s family
    /// and starts answering.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if the config is invalid;
    /// [`ControlError::Network`] if binding fails (the message names the
    /// port, never an IP).
    pub async fn start(config: &DnsConfig) -> Result<Self> {
        config.validate()?;
        let (udp, tcp) = bind_pair(wildcard_of(config.bind), config.port).await?;
        let listen_addr = udp.local_addr().map_err(|e| bind_error(config.port, &e))?;
        tracing::info!(
            port = listen_addr.port(),
            upstream = %config.upstream,
            names = %config
                .overrides
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
            "dns: resolver started"
        );
        let resolver = Arc::new(Resolver::new(config.clone()));
        let tasks = vec![
            tokio::spawn(udp_loop(Arc::new(udp), Arc::clone(&resolver))),
            tokio::spawn(tcp_loop(tcp, resolver)),
        ];
        Ok(Self { listen_addr, tasks })
    }

    /// The sockets actually listened on: the wildcard address of
    /// [`DnsConfig::bind`]'s family and the bound port (UDP and TCP).
    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Stops answering, closes every connection and waits until the sockets
    /// are closed.
    pub async fn shutdown(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
            // A cancelled task is the expected outcome.
            let _ = task.await;
        }
        tracing::info!("dns: resolver stopped");
    }
}

impl Drop for DnsServer {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Binds UDP and TCP on the same port; with port 0, retries until the port
/// the UDP socket got is also free for TCP.
async fn bind_pair(ip: IpAddr, port: u16) -> Result<(UdpSocket, TcpListener)> {
    let attempts = if port == 0 {
        EPHEMERAL_BIND_ATTEMPTS
    } else {
        1
    };
    let mut last = None;
    for _ in 0..attempts {
        let udp = UdpSocket::bind(SocketAddr::new(ip, port))
            .await
            .map_err(|e| bind_error(port, &e))?;
        let udp_port = udp.local_addr().map_err(|e| bind_error(port, &e))?.port();
        match TcpListener::bind(SocketAddr::new(ip, udp_port)).await {
            Ok(tcp) => return Ok((udp, tcp)),
            Err(e) => last = Some(e),
        }
    }
    let e = last.unwrap_or_else(|| io::Error::other("no bind attempt"));
    Err(bind_error(port, &e))
}

/// The unspecified address of `ip`'s family (`0.0.0.0` or `::`).
fn wildcard_of(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

/// Maps a bind failure to an error naming the port only.
fn bind_error(port: u16, e: &io::Error) -> ControlError {
    if e.kind() == io::ErrorKind::PermissionDenied {
        ControlError::network(format!(
            "local DNS bind: permission denied for port {port} on the wildcard address \
             (macOS lets unprivileged processes bind ports below 1024 only on the \
             wildcard address): {e}"
        ))
    } else {
        ControlError::network(format!("local DNS bind on port {port}: {e}"))
    }
}

/// How a query reached us (and so how it is forwarded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Udp,
    Tcp,
}

/// A query that passed the checks.
#[derive(Debug, PartialEq, Eq)]
struct Question {
    /// Lowercase, without the trailing dot.
    name: String,
    qtype: RecordType,
    qclass: DNSClass,
    /// Offset just past the question section in the raw query.
    end: usize,
    /// The largest UDP reply the client accepts (512 without EDNS).
    udp_limit: usize,
}

/// What to do with one incoming message.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Send this reply.
    Reply(Vec<u8>),
    /// Relay the raw query upstream.
    Forward(Question),
    /// Ignore it.
    Drop,
}

/// The shared state of one resolver.
struct Resolver {
    config: DnsConfig,
    in_flight: Arc<Semaphore>,
    logged: Mutex<HashSet<(IpAddr, String, u16)>>,
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Resolver {
    fn new(config: DnsConfig) -> Self {
        Self {
            config,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            logged: Mutex::new(HashSet::new()),
        }
    }

    /// Decides what to do with `query` from `peer` (already subnet-checked).
    fn decide(&self, query: &[u8], peer: IpAddr) -> Action {
        let Some(question) = parse_query(query) else {
            return Action::Drop;
        };
        let overridden = self.config.is_overridden(&question.name);
        self.note(peer, &question, overridden);
        if !overridden {
            return Action::Forward(question);
        }
        let answer = (question.qclass == DNSClass::IN
            && question.qtype == family_type(self.config.bind))
        .then_some(self.config.bind);
        Action::Reply(build_reply(
            query,
            question.end,
            ReplyKind::Answer {
                address: answer,
                ttl: self.config.answer_ttl,
            },
        ))
    }

    /// Logs the first sighting of (peer, name, type).
    fn note(&self, peer: IpAddr, question: &Question, overridden: bool) {
        let key = (peer, question.name.clone(), u16::from(question.qtype));
        let first = {
            let mut logged = self.logged.lock().unwrap_or_else(PoisonError::into_inner);
            if logged.len() >= MAX_LOGGED {
                false
            } else {
                let first = logged.insert(key);
                if first && logged.len() == MAX_LOGGED {
                    tracing::warn!(
                        max = MAX_LOGGED,
                        "dns: query log is full; new names are no longer logged"
                    );
                }
                first
            }
        };
        if !first {
            return;
        }
        let action = if overridden { "override" } else { "forward" };
        if is_interesting(&question.name, overridden) {
            tracing::info!(%peer, name = %question.name, qtype = %question.qtype, action, "dns: query");
        } else {
            tracing::debug!(%peer, name = %question.name, qtype = %question.qtype, action, "dns: query");
        }
    }

    /// Relays `query` upstream and returns the reply for the client: the
    /// upstream's reply, a header-only TC reply when it is too large for the
    /// client's UDP limit, or SERVFAIL.
    async fn forward(&self, query: &[u8], question: &Question, transport: Transport) -> Vec<u8> {
        let Ok(_permit) = self.in_flight.try_acquire() else {
            tracing::warn!(max = MAX_IN_FLIGHT, "dns: too many queries in flight");
            return build_reply(query, question.end, ReplyKind::ServFail);
        };
        let upstream = self.config.upstream;
        let mut reply = None;
        if transport == Transport::Udp {
            for _ in 0..2 {
                match forward_udp(upstream, query).await {
                    Ok(r) => {
                        reply = Some(r);
                        break;
                    }
                    Err(e) => tracing::debug!(error = %e, "dns: upstream UDP attempt failed"),
                }
            }
        }
        if reply.is_none() {
            match forward_tcp(upstream, query).await {
                Ok(r) => reply = Some(r),
                Err(e) => tracing::debug!(error = %e, "dns: upstream TCP attempt failed"),
            }
        }
        match reply {
            Some(r) if transport == Transport::Udp && r.len() > question.udp_limit => {
                build_reply(query, question.end, ReplyKind::Truncated)
            }
            Some(r) => r,
            None => {
                tracing::debug!(name = %question.name, "dns: upstream failed; answering SERVFAIL");
                build_reply(query, question.end, ReplyKind::ServFail)
            }
        }
    }
}

/// Names worth an info line: ours, LG's and those of AWS `IoT`.
fn is_interesting(name: &str, overridden: bool) -> bool {
    overridden || name.contains("lgthinq") || name.contains("amazonaws")
}

/// The address record type of `ip`'s family.
fn family_type(ip: IpAddr) -> RecordType {
    match ip {
        IpAddr::V4(_) => RecordType::A,
        IpAddr::V6(_) => RecordType::AAAA,
    }
}

/// Parses a standard query with exactly one question; `None` for anything
/// else (responses, other opcodes, malformed or compressed questions).
fn parse_query(packet: &[u8]) -> Option<Question> {
    let message = Message::from_vec(packet).ok()?;
    if message.metadata.message_type != MessageType::Query
        || message.metadata.op_code != OpCode::Query
        || message.queries.len() != 1
    {
        return None;
    }
    let query = message.queries.first()?;
    Some(Question {
        name: normalize(&query.name().to_ascii()),
        qtype: query.query_type(),
        qclass: query.query_class(),
        end: question_end(packet)?,
        udp_limit: usize::from(message.max_payload()).min(MAX_UDP_BYTES),
    })
}

/// The offset just past the first question: uncompressed labels, the root
/// label, QTYPE and QCLASS.
fn question_end(packet: &[u8]) -> Option<usize> {
    let mut at = HEADER_LEN;
    loop {
        let len = usize::from(*packet.get(at)?);
        if len == 0 {
            at += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None;
        }
        at += 1 + len;
    }
    let end = at + 4;
    (end <= packet.len()).then_some(end)
}

/// The replies we build ourselves.
#[derive(Clone, Copy, Debug)]
enum ReplyKind {
    /// Authoritative NOERROR, with one address record or none.
    Answer { address: Option<IpAddr>, ttl: u32 },
    /// SERVFAIL.
    ServFail,
    /// NOERROR with TC set and no records: retry over TCP.
    Truncated,
}

/// Builds a reply to `query` whose question section ends at `question_end`:
/// the id, opcode and RD bit copied, RA set, the question copied byte for
/// byte, and at most one answer pointing back at it.
fn build_reply(query: &[u8], question_end: usize, kind: ReplyKind) -> Vec<u8> {
    let (rcode, authoritative, truncated, address, ttl) = match kind {
        ReplyKind::Answer { address, ttl } => (RCODE_NOERROR, true, false, address, ttl),
        ReplyKind::ServFail => (RCODE_SERVFAIL, false, false, None, 0),
        ReplyKind::Truncated => (RCODE_NOERROR, false, true, None, 0),
    };
    let mut reply = Vec::with_capacity(question_end + 28);
    reply.extend_from_slice(&query[..2]);
    let mut flags_hi = 0x80 | (query[2] & 0x79); // QR, opcode, RD
    if authoritative {
        flags_hi |= 0x04;
    }
    if truncated {
        flags_hi |= 0x02;
    }
    reply.push(flags_hi);
    reply.push(0x80 | rcode); // RA
    reply.extend_from_slice(&[0, 1]); // QDCOUNT
    reply.extend_from_slice(&[0, u8::from(address.is_some())]); // ANCOUNT
    reply.extend_from_slice(&[0, 0, 0, 0]); // NSCOUNT, ARCOUNT
    reply.extend_from_slice(&query[HEADER_LEN..question_end]);
    if let Some(address) = address {
        reply.extend_from_slice(&[0xC0, 0x0C]); // name: pointer to the question
        reply.extend_from_slice(&u16::from(family_type(address)).to_be_bytes());
        reply.extend_from_slice(&u16::from(DNSClass::IN).to_be_bytes());
        reply.extend_from_slice(&ttl.to_be_bytes());
        match address {
            IpAddr::V4(v4) => {
                reply.extend_from_slice(&4u16.to_be_bytes());
                reply.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                reply.extend_from_slice(&16u16.to_be_bytes());
                reply.extend_from_slice(&v6.octets());
            }
        }
    }
    reply
}

/// One UDP exchange with `upstream` from a fresh connected socket; only a
/// reply with the query's id counts.
async fn forward_udp(upstream: SocketAddr, query: &[u8]) -> io::Result<Vec<u8>> {
    let socket = UdpSocket::bind(SocketAddr::new(wildcard_of(upstream.ip()), 0)).await?;
    socket.connect(upstream).await?;
    let exchange = async {
        socket.send(query).await?;
        let mut buf = vec![0; MAX_UDP_BYTES + 1];
        loop {
            let len = socket.recv(&mut buf).await?;
            if len > MAX_UDP_BYTES {
                return Err(io::Error::other("upstream UDP reply too large"));
            }
            if len >= HEADER_LEN && buf[..2] == query[..2] {
                buf.truncate(len);
                return Ok(buf);
            }
        }
    };
    tokio::time::timeout(UPSTREAM_TIMEOUT, exchange)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "upstream UDP timed out"))?
}

/// One TCP exchange with `upstream`, connect included.
async fn forward_tcp(upstream: SocketAddr, query: &[u8]) -> io::Result<Vec<u8>> {
    let exchange = async {
        let mut stream = TcpStream::connect(upstream).await?;
        write_frame(&mut stream, query).await?;
        let reply = read_frame(&mut stream).await?;
        if reply.len() < HEADER_LEN || reply[..2] != query[..2] {
            return Err(io::Error::other("upstream TCP reply has another id"));
        }
        Ok(reply)
    };
    tokio::time::timeout(UPSTREAM_TIMEOUT, exchange)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "upstream TCP timed out"))?
}

/// Writes one length-prefixed DNS message.
async fn write_frame(stream: &mut TcpStream, message: &[u8]) -> io::Result<()> {
    let len = u16::try_from(message.len())
        .map_err(|_| io::Error::other("DNS message too large for TCP"))?;
    let mut frame = Vec::with_capacity(message.len() + 2);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(message);
    stream.write_all(&frame).await
}

/// Reads one length-prefixed DNS message (at most 65535 bytes by framing).
async fn read_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let len = usize::from(stream.read_u16().await?);
    let mut message = vec![0; len];
    stream.read_exact(&mut message).await?;
    Ok(message)
}

/// Answers UDP queries until aborted; aborting drops the `JoinSet`, which
/// aborts every forward in progress.
async fn udp_loop(socket: Arc<UdpSocket>, resolver: Arc<Resolver>) {
    let mut forwards = JoinSet::new();
    let mut buf = vec![0; MAX_UDP_BYTES + 1];
    loop {
        while forwards.try_join_next().is_some() {}
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(e) => {
                tracing::debug!(error = %e, "dns: UDP receive failed");
                tokio::time::sleep(ERROR_PAUSE).await;
                continue;
            }
        };
        if !same_subnet(resolver.config.bind, peer.ip()) {
            tracing::debug!("dns: dropped a query from outside the LAN subnet");
            continue;
        }
        if len > MAX_UDP_BYTES {
            continue;
        }
        let query = buf[..len].to_vec();
        match resolver.decide(&query, peer.ip()) {
            Action::Drop => {}
            Action::Reply(reply) => send_udp(&socket, &reply, peer).await,
            Action::Forward(question) => {
                if resolver.in_flight.available_permits() == 0 {
                    tracing::warn!(max = MAX_IN_FLIGHT, "dns: too many queries in flight");
                    continue;
                }
                let socket = Arc::clone(&socket);
                let resolver = Arc::clone(&resolver);
                forwards.spawn(async move {
                    let reply = resolver.forward(&query, &question, Transport::Udp).await;
                    send_udp(&socket, &reply, peer).await;
                });
            }
        }
    }
}

async fn send_udp(socket: &UdpSocket, reply: &[u8], peer: SocketAddr) {
    if let Err(e) = socket.send_to(reply, peer).await {
        tracing::debug!(error = %e, "dns: UDP send failed");
    }
}

/// Accepts TCP connections until aborted; aborting drops the `JoinSet`,
/// which aborts every connection task.
async fn tcp_loop(listener: TcpListener, resolver: Arc<Resolver>) {
    let permits = Arc::new(Semaphore::new(MAX_TCP_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let stream = match listener.accept().await {
            Ok((stream, peer)) if same_subnet(resolver.config.bind, peer.ip()) => {
                (stream, peer.ip())
            }
            Ok(_) => {
                tracing::debug!("dns: dropped a TCP connection from outside the LAN subnet");
                continue;
            }
            Err(e) => {
                tracing::debug!(error = %e, "dns: TCP accept failed");
                tokio::time::sleep(ERROR_PAUSE).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            tracing::warn!(
                max = MAX_TCP_CONNECTIONS,
                "dns: too many TCP connections; closing a new one"
            );
            continue;
        };
        let resolver = Arc::clone(&resolver);
        connections.spawn(async move {
            let _permit = permit;
            let (stream, peer) = stream;
            serve_tcp(stream, peer, &resolver).await;
        });
    }
}

/// Answers queries on one TCP connection, one at a time, until the peer
/// closes it, sends something malformed or stays idle too long.
async fn serve_tcp(mut stream: TcpStream, peer: IpAddr, resolver: &Resolver) {
    loop {
        let Ok(Ok(query)) = tokio::time::timeout(TCP_IDLE_TIMEOUT, read_frame(&mut stream)).await
        else {
            return;
        };
        let reply = match resolver.decide(&query, peer) {
            Action::Drop => return,
            Action::Reply(reply) => reply,
            Action::Forward(question) => resolver.forward(&query, &question, Transport::Tcp).await,
        };
        let written =
            tokio::time::timeout(TCP_IDLE_TIMEOUT, write_frame(&mut stream, &reply)).await;
        if !matches!(written, Ok(Ok(()))) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::{Edns, Query, ResponseCode};
    use hickory_proto::rr::{Name, RData};

    const BIND: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5)); // privacy:allow (TEST-NET-1)
    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9)); // privacy:allow (TEST-NET-1)
    const UPSTREAM_ANSWER: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7); // privacy:allow (TEST-NET-3)
    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn query(id: u16, name: &str, qtype: RecordType) -> Vec<u8> {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.metadata.recursion_desired = true;
        message.add_query(Query::query(Name::from_ascii(name).unwrap(), qtype));
        message.to_vec().unwrap()
    }

    fn resolver(bind: IpAddr) -> Resolver {
        Resolver::new(DnsConfig::new(bind, DEFAULT_UPSTREAM))
    }

    fn reply_of(action: Action) -> Message {
        let Action::Reply(bytes) = action else {
            panic!("not a reply: {action:?}");
        };
        Message::from_vec(&bytes).unwrap()
    }

    fn addresses(message: &Message) -> Vec<IpAddr> {
        message
            .answers
            .iter()
            .map(|r| match &r.data {
                RData::A(a) => IpAddr::V4(a.0),
                RData::AAAA(a) => IpAddr::V6(a.0),
                other => panic!("unexpected record {other:?}"),
            })
            .collect()
    }

    #[test]
    fn name_rules_parse_and_match() {
        let exact = NameRule::parse("Common.LGThinQ.com.").unwrap();
        assert_eq!(exact, NameRule::Exact("common.lgthinq.com".to_owned()));
        assert!(exact.matches("common.lgthinq.com"));
        assert!(!exact.matches("x.common.lgthinq.com"));
        let suffix = NameRule::parse(".LGThinQ.com").unwrap();
        assert_eq!(suffix, NameRule::Suffix("lgthinq.com".to_owned()));
        assert_eq!(suffix.to_string(), ".lgthinq.com");
        assert!(suffix.matches("eic-common.lgthinq.com"));
        assert!(suffix.matches("a.b.lgthinq.com"));
        assert!(!suffix.matches("lgthinq.com"), "the apex is not *.domain");
        assert!(!suffix.matches("notlgthinq.com"));
        assert!(!suffix.matches(".lgthinq.com"));
        for bad in ["", ".", "..x", "a..b", "a b.com", "*.x.com", "-.com\u{e9}"] {
            assert!(NameRule::parse(bad).is_err(), "{bad:?}");
        }
        assert!(NameRule::parse(&"a".repeat(64)).is_err());
        assert!(NameRule::parse("_srv.ac-1.example.org").is_ok());
    }

    #[test]
    fn default_names_cover_the_setup_hosts() {
        let config = DnsConfig::new(BIND, DEFAULT_UPSTREAM);
        for name in [
            "common.lgthinq.com",
            "comon.lgthinq.com",
            "kic-common.lgthinq.com",
        ] {
            assert!(config.is_overridden(name), "{name}");
        }
        for name in [
            "example.org",
            "lgthinq.com.example.org",
            "a1b2.iot.amazonaws.com",
        ] {
            assert!(!config.is_overridden(name), "{name}");
        }
        assert_eq!(config.upstream, "8.8.8.8:53".parse().unwrap());
        assert_eq!(config.port, 53);
        assert_eq!(config.answer_ttl, 60);
    }

    #[test]
    fn config_validation() {
        let good = DnsConfig::new(BIND, DEFAULT_UPSTREAM);
        assert!(good.validate().is_ok());
        let cases = [
            DnsConfig {
                bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                ..good.clone()
            },
            DnsConfig {
                upstream: SocketAddr::new(BIND, DNS_PORT),
                ..good.clone()
            },
            DnsConfig {
                upstream: "0.0.0.0:53".parse().unwrap(),
                ..good.clone()
            },
            DnsConfig {
                answer_ttl: 0,
                ..good.clone()
            },
        ];
        for bad in cases {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn overridden_a_is_answered_with_bind() {
        let raw = query(0x1234, "CoMmOn.LgThInQ.cOm.", RecordType::A);
        let action = resolver(BIND).decide(&raw, PEER);
        let Action::Reply(bytes) = &action else {
            panic!("{action:?}");
        };
        let end = question_end(&raw).unwrap();
        assert_eq!(
            bytes[HEADER_LEN..end],
            raw[HEADER_LEN..end],
            "question byte-exact"
        );
        let reply = reply_of(action);
        assert_eq!(reply.metadata.id, 0x1234);
        assert_eq!(reply.metadata.message_type, MessageType::Response);
        assert!(reply.metadata.authoritative);
        assert!(reply.metadata.recursion_desired);
        assert!(!reply.metadata.truncation);
        assert_eq!(reply.metadata.response_code, ResponseCode::NoError);
        assert_eq!(reply.queries.len(), 1);
        assert_eq!(reply.queries[0].name().to_ascii(), "CoMmOn.LgThInQ.cOm.");
        assert_eq!(addresses(&reply), [BIND]);
        assert_eq!(reply.answers[0].ttl, ANSWER_TTL);
        assert_eq!(reply.answers[0].name.to_ascii(), "CoMmOn.LgThInQ.cOm.");
    }

    #[test]
    fn other_types_of_overridden_names_are_empty() {
        for qtype in [RecordType::AAAA, RecordType::TXT, RecordType::HTTPS] {
            let reply =
                reply_of(resolver(BIND).decide(&query(7, "comon.lgthinq.com.", qtype), PEER));
            assert_eq!(
                reply.metadata.response_code,
                ResponseCode::NoError,
                "{qtype}"
            );
            assert!(reply.metadata.authoritative);
            assert_eq!(reply.answers, [], "{qtype}");
        }
    }

    #[test]
    fn ipv6_bind_answers_aaaa() {
        let v6: IpAddr = "2001:db8::5".parse().unwrap();
        let r = resolver(v6);
        let aaaa = reply_of(r.decide(&query(1, "common.lgthinq.com.", RecordType::AAAA), PEER));
        assert_eq!(addresses(&aaaa), [v6]);
        let a = reply_of(r.decide(&query(2, "common.lgthinq.com.", RecordType::A), PEER));
        assert_eq!(a.answers, []);
    }

    #[test]
    fn other_names_are_forwarded() {
        let action = resolver(BIND).decide(&query(9, "Example.org.", RecordType::A), PEER);
        let Action::Forward(question) = action else {
            panic!("{action:?}");
        };
        assert_eq!(question.name, "example.org");
        assert_eq!(question.qtype, RecordType::A);
        assert_eq!(question.udp_limit, 512);
    }

    #[test]
    fn edns_raises_the_udp_limit_up_to_our_buffer() {
        let mut message = Message::new(3, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_ascii("example.org.").unwrap(),
            RecordType::A,
        ));
        let mut edns = Edns::new();
        edns.set_max_payload(65000);
        message.set_edns(edns);
        let question = parse_query(&message.to_vec().unwrap()).unwrap();
        assert_eq!(question.udp_limit, MAX_UDP_BYTES);
    }

    #[test]
    fn malformed_and_unexpected_messages_are_dropped() {
        let r = resolver(BIND);
        let mut two = Message::new(4, MessageType::Query, OpCode::Query);
        two.add_query(Query::query(
            Name::from_ascii("a.example.org.").unwrap(),
            RecordType::A,
        ));
        two.add_query(Query::query(
            Name::from_ascii("b.example.org.").unwrap(),
            RecordType::A,
        ));
        let mut response = Message::new(5, MessageType::Response, OpCode::Query);
        response.add_query(Query::query(
            Name::from_ascii("common.lgthinq.com.").unwrap(),
            RecordType::A,
        ));
        let mut update = Message::new(6, MessageType::Query, OpCode::Update);
        update.add_query(Query::query(
            Name::from_ascii("common.lgthinq.com.").unwrap(),
            RecordType::A,
        ));
        let mut none = Message::new(8, MessageType::Query, OpCode::Query);
        none.metadata.recursion_desired = true;
        let good = query(10, "common.lgthinq.com.", RecordType::A);
        let cases = [
            Vec::new(),
            vec![0; 5],
            good[..good.len() - 1].to_vec(),
            two.to_vec().unwrap(),
            response.to_vec().unwrap(),
            update.to_vec().unwrap(),
            none.to_vec().unwrap(),
        ];
        for (i, bytes) in cases.iter().enumerate() {
            assert_eq!(r.decide(bytes, PEER), Action::Drop, "case {i}");
        }
    }

    #[test]
    fn servfail_and_truncated_replies_keep_the_question() {
        let raw = query(0xBEEF, "example.org.", RecordType::A);
        let end = question_end(&raw).unwrap();
        let servfail = Message::from_vec(&build_reply(&raw, end, ReplyKind::ServFail)).unwrap();
        assert_eq!(servfail.metadata.id, 0xBEEF);
        assert_eq!(servfail.metadata.response_code, ResponseCode::ServFail);
        assert!(!servfail.metadata.authoritative);
        assert_eq!(servfail.queries.len(), 1);
        let truncated = Message::from_vec(&build_reply(&raw, end, ReplyKind::Truncated)).unwrap();
        assert!(truncated.metadata.truncation);
        assert_eq!(truncated.metadata.response_code, ResponseCode::NoError);
        assert_eq!(truncated.answers, []);
    }

    #[test]
    fn interesting_names_are_classified() {
        assert!(is_interesting("common.lgthinq.com", true));
        assert!(is_interesting("my.example.org", true));
        assert!(is_interesting("aic.lgthinq.com.example.org", false));
        assert!(is_interesting(
            "a1b2-ats.iot.eu-west-1.amazonaws.com",
            false
        ));
        assert!(!is_interesting("example.org", false));
    }

    #[test]
    fn query_log_set_is_capped() {
        let r = resolver(BIND);
        for i in 0..MAX_LOGGED + 10 {
            let raw = query(1, &format!("n{i}.example.org."), RecordType::A);
            let _ = r.decide(&raw, PEER);
        }
        assert_eq!(r.logged.lock().unwrap().len(), MAX_LOGGED);
    }

    // --- loopback integration: a fake upstream on 127.0.0.1 ---------------

    /// What the fake upstream does with each query.
    #[derive(Clone, Copy)]
    enum Upstream {
        /// Answers A = `UPSTREAM_ANSWER` over UDP and TCP.
        Answer,
        /// Over UDP sends a reply with the wrong id first, then the right one.
        WrongIdFirst,
        /// Over UDP replies with TC set and nothing else.
        Truncate,
    }

    fn upstream_reply(raw: &[u8], truncate: bool) -> Vec<u8> {
        let request = Message::from_vec(raw).unwrap();
        let mut reply = Message::response(request.metadata.id, OpCode::Query);
        reply.add_query(request.queries[0].clone());
        if truncate {
            reply.metadata.truncation = true;
        } else {
            reply.add_answer(hickory_proto::rr::Record::from_rdata(
                request.queries[0].name().clone(),
                300,
                RData::A(hickory_proto::rr::rdata::A(UPSTREAM_ANSWER)),
            ));
        }
        reply.to_vec().unwrap()
    }

    /// Starts a fake upstream on 127.0.0.1 (UDP and TCP on one port).
    async fn fake_upstream(mode: Upstream) -> (SocketAddr, JoinHandle<()>) {
        let (udp, tcp) = bind_pair(LOOPBACK, 0).await.unwrap();
        let addr = udp.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let udp_task = async {
                let mut buf = vec![0; 4096];
                loop {
                    let (len, peer) = udp.recv_from(&mut buf).await.unwrap();
                    let raw = buf[..len].to_vec();
                    match mode {
                        Upstream::Answer => {}
                        Upstream::WrongIdFirst => {
                            // Another id and no answers: must be ignored.
                            let mut wrong = upstream_reply(&raw, true);
                            wrong[0] ^= 0xFF;
                            udp.send_to(&wrong, peer).await.unwrap();
                        }
                        Upstream::Truncate => {
                            udp.send_to(&upstream_reply(&raw, true), peer)
                                .await
                                .unwrap();
                            continue;
                        }
                    }
                    udp.send_to(&upstream_reply(&raw, false), peer)
                        .await
                        .unwrap();
                }
            };
            let tcp_task = async {
                loop {
                    let (mut stream, _) = tcp.accept().await.unwrap();
                    tokio::spawn(async move {
                        while let Ok(raw) = read_frame(&mut stream).await {
                            write_frame(&mut stream, &upstream_reply(&raw, false))
                                .await
                                .unwrap();
                        }
                    });
                }
            };
            tokio::join!(udp_task, tcp_task);
        });
        (addr, task)
    }

    /// A free loopback port nobody listens on (UDP and TCP).
    async fn dead_upstream() -> SocketAddr {
        let (udp, _tcp) = bind_pair(LOOPBACK, 0).await.unwrap();
        udp.local_addr().unwrap()
    }

    async fn start_server(bind: IpAddr, upstream: SocketAddr) -> DnsServer {
        let config = DnsConfig {
            bind,
            port: 0,
            upstream,
            overrides: default_overrides(),
            answer_ttl: ANSWER_TTL,
        };
        DnsServer::start(&config).await.unwrap()
    }

    fn server_addr(server: &DnsServer) -> SocketAddr {
        SocketAddr::new(LOOPBACK, server.listen_addr().port())
    }

    async fn ask_udp(server: SocketAddr, raw: &[u8], wait: Duration) -> Option<Message> {
        let socket = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
        socket.connect(server).await.unwrap();
        socket.send(raw).await.unwrap();
        let mut buf = vec![0; 65535];
        let len = tokio::time::timeout(wait, socket.recv(&mut buf))
            .await
            .ok()?
            .ok()?;
        Some(Message::from_vec(&buf[..len]).unwrap())
    }

    async fn ask_tcp(server: SocketAddr, queries: &[Vec<u8>]) -> Vec<Message> {
        let mut stream = TcpStream::connect(server).await.unwrap();
        let mut replies = Vec::new();
        for raw in queries {
            write_frame(&mut stream, raw).await.unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream))
                .await
                .unwrap()
                .unwrap();
            replies.push(Message::from_vec(&reply).unwrap());
        }
        replies
    }

    const WAIT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn udp_answers_overrides_and_forwards_others() {
        let (upstream, _fake) = fake_upstream(Upstream::Answer).await;
        let server = start_server(LOOPBACK, upstream).await;
        assert!(server.listen_addr().ip().is_unspecified());
        let addr = server_addr(&server);
        let ours = ask_udp(addr, &query(1, "common.lgthinq.com.", RecordType::A), WAIT)
            .await
            .unwrap();
        assert_eq!(addresses(&ours), [LOOPBACK]);
        assert!(ours.metadata.authoritative);
        let other = ask_udp(addr, &query(2, "example.org.", RecordType::A), WAIT)
            .await
            .unwrap();
        assert_eq!(other.metadata.id, 2);
        assert_eq!(addresses(&other), [IpAddr::V4(UPSTREAM_ANSWER)]);
        assert!(!other.metadata.authoritative);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn tcp_answers_overrides_and_forwards_others() {
        let (upstream, _fake) = fake_upstream(Upstream::Answer).await;
        let server = start_server(LOOPBACK, upstream).await;
        let replies = ask_tcp(
            server_addr(&server),
            &[
                query(1, "comon.lgthinq.com.", RecordType::A),
                query(2, "example.org.", RecordType::A),
            ],
        )
        .await;
        assert_eq!(addresses(&replies[0]), [LOOPBACK]);
        assert_eq!(addresses(&replies[1]), [IpAddr::V4(UPSTREAM_ANSWER)]);
        assert_eq!(replies[1].metadata.id, 2);
    }

    #[tokio::test]
    async fn upstream_replies_with_another_id_are_ignored() {
        let (upstream, _fake) = fake_upstream(Upstream::WrongIdFirst).await;
        let server = start_server(LOOPBACK, upstream).await;
        let reply = ask_udp(
            server_addr(&server),
            &query(0x4242, "example.org.", RecordType::A),
            WAIT,
        )
        .await
        .unwrap();
        assert_eq!(reply.metadata.id, 0x4242);
        assert_eq!(addresses(&reply), [IpAddr::V4(UPSTREAM_ANSWER)]);
    }

    #[tokio::test]
    async fn truncated_upstream_replies_pass_through() {
        let (upstream, _fake) = fake_upstream(Upstream::Truncate).await;
        let server = start_server(LOOPBACK, upstream).await;
        let reply = ask_udp(
            server_addr(&server),
            &query(5, "example.org.", RecordType::A),
            WAIT,
        )
        .await
        .unwrap();
        assert!(reply.metadata.truncation);
        assert_eq!(reply.answers, []);
    }

    #[tokio::test]
    async fn dead_upstream_gives_servfail() {
        let server = start_server(LOOPBACK, dead_upstream().await).await;
        // UDP to a closed loopback port is refused at once, and so is TCP.
        let reply = ask_udp(
            server_addr(&server),
            &query(6, "example.org.", RecordType::A),
            WAIT,
        )
        .await
        .unwrap();
        assert_eq!(reply.metadata.id, 6);
        assert_eq!(reply.metadata.response_code, ResponseCode::ServFail);
        let replies = ask_tcp(
            server_addr(&server),
            &[query(7, "example.org.", RecordType::A)],
        )
        .await;
        assert_eq!(replies[0].metadata.response_code, ResponseCode::ServFail);
        // Overrides need no upstream.
        let ours = ask_udp(
            server_addr(&server),
            &query(8, "common.lgthinq.com.", RecordType::A),
            WAIT,
        )
        .await
        .unwrap();
        assert_eq!(addresses(&ours), [LOOPBACK]);
    }

    #[tokio::test]
    async fn peers_outside_the_subnet_are_dropped() {
        let (upstream, _fake) = fake_upstream(Upstream::Answer).await;
        // Loopback peers are outside 192.0.2.0/24.
        let server = start_server(BIND, upstream).await;
        let addr = server_addr(&server);
        let reply = ask_udp(
            addr,
            &query(1, "common.lgthinq.com.", RecordType::A),
            Duration::from_millis(300),
        )
        .await;
        assert!(reply.is_none(), "{reply:?}");
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let _ = write_frame(&mut stream, &query(2, "common.lgthinq.com.", RecordType::A)).await;
        let read = tokio::time::timeout(WAIT, read_frame(&mut stream))
            .await
            .unwrap();
        assert!(read.is_err(), "the connection is closed without a reply");
    }

    #[tokio::test]
    async fn dropping_the_server_stops_it() {
        let (upstream, _fake) = fake_upstream(Upstream::Answer).await;
        let server = start_server(LOOPBACK, upstream).await;
        let addr = server_addr(&server);
        assert!(
            ask_udp(addr, &query(1, "common.lgthinq.com.", RecordType::A), WAIT)
                .await
                .is_some()
        );
        drop(server);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let reply = ask_udp(
            addr,
            &query(2, "common.lgthinq.com.", RecordType::A),
            Duration::from_millis(300),
        )
        .await;
        assert!(reply.is_none());
        assert!(TcpStream::connect(addr).await.is_err());
    }

    #[tokio::test]
    async fn bind_errors_name_the_port_only() {
        let (udp, _tcp) = bind_pair(LOOPBACK, 0).await.unwrap();
        let port = udp.local_addr().unwrap().port();
        let err = bind_pair(LOOPBACK, port).await.unwrap_err().to_string();
        assert!(err.contains(&format!("port {port}")), "{err}");
        assert!(!err.contains("127.0.0.1"), "{err}");
        let denied = bind_error(53, &io::Error::from(io::ErrorKind::PermissionDenied)).to_string();
        assert!(denied.contains("permission denied for port 53"), "{denied}");
        assert!(denied.contains("wildcard"), "{denied}");
    }

    /// Opt-in (binds the real port 53 for well under a second; sends
    /// nothing): whether an unprivileged process may bind UDP and TCP 53 on
    /// macOS. Run with
    /// `cargo nextest run -p controlz_core --run-ignored only dns::tests::binds_port_53_unprivileged`.
    #[tokio::test]
    #[ignore = "binds the real port 53"]
    async fn binds_port_53_unprivileged() {
        let wildcard = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DNS_PORT);
        let udp = UdpSocket::bind(wildcard).await;
        let tcp = TcpListener::bind(wildcard).await;
        assert!(udp.is_ok(), "UDP 0.0.0.0:53: {:?}", udp.err());
        assert!(tcp.is_ok(), "TCP 0.0.0.0:53: {:?}", tcp.err());
        drop((udp, tcp));
        let specific = SocketAddr::new(LOOPBACK, DNS_PORT);
        let udp = UdpSocket::bind(specific).await.map(drop);
        let tcp = TcpListener::bind(specific).await.map(drop);
        assert_eq!(
            udp.as_ref().map_err(io::Error::kind),
            Err(io::ErrorKind::PermissionDenied),
            "UDP 127.0.0.1:53: {udp:?}"
        );
        assert_eq!(
            tcp.as_ref().map_err(io::Error::kind),
            Err(io::ErrorKind::PermissionDenied),
            "TCP 127.0.0.1:53: {tcp:?}"
        );
    }
}
