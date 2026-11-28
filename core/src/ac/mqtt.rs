//! Single-device MQTT broker (KB: AC — local control options, local cloud side).
//!
//! A clean-room MQTT 3.1.1 subset over TLS for the one module being
//! provisioned (rethink wiki, `Thinq2CloudProtocol`; MQTT 3.1.1 OASIS spec):
//! CONNECT/CONNACK, SUBSCRIBE/SUBACK, UNSUBSCRIBE/UNSUBACK, PUBLISH at `QoS` 0
//! and 1 with PUBACK both ways, PINGREQ/PINGRESP and DISCONNECT. `QoS` 2,
//! reserved packet types and malformed packets close the connection.
//!
//! Message flow: the module publishes `preDeploy` or `deploy` on its
//! provisioning topic; we answer `completeProvisioning` on
//! `lime/devices/<uuid>` with LG's default topic names
//! ([`PROVISIONING_TOPIC_PREFIX`], [`MESSAGE_TOPIC_PREFIX`]); the module
//! confirms with `completeProvisioning_ack` ([`DeviceEvent::Provisioned`]).
//! Then state arrives as `device_packet` ([`DeviceEvent::DevicePacket`]) and
//! commands go out as `packet` ([`MqttBroker::send_packet`]). Device
//! publishes are accepted on the `$aws/rules/...` topics we hand out and on
//! their bare `clip/...` forms; any known command is accepted on any of them.
//!
//! Hardening: the TLS handshake requires a client certificate and proof of
//! its private key ([`tls::mqtt_acceptor`]). A certificate from our client
//! CA (the CSR path) is accepted as before. Any other issuer is accepted
//! only by its pinned public key (D21, [`Pinning`]): the module keeps the
//! certificate of its `ThinQ` registration (seen: AWS IoT-issued, with a
//! generic wildcard common name, so the CN is only logged). Such a
//! certificate is checked (pin or pinning window) before any MQTT byte is
//! parsed, and pinned only after a CONNECT whose `clientId` is the
//! session's uuid. With our client CA, CONNECT is accepted only if the
//! certificate's common name, the `clientId` and the session's uuid are
//! all equal. One device session at a
//! time: a new valid CONNECT takes over and closes the old connection.
//! Connections are capped; the CONNECT, every packet, idle periods and
//! every write are time-bounded; packets, topics and subscriptions are
//! size-bounded. Logs never contain the uuid or topic uuids; JSON payloads
//! are logged only at trace level, with the uuid and identifying fields
//! (`did`, `mac`, `ssid`, …) replaced by `***`.
//!
//! Device quirks handled (T1.5 checks for T1.9): a `preDeploy`/`deploy` that
//! arrives before the module subscribes to `lime/devices/<uuid>` is answered
//! right after that SUBSCRIBE; a device message without `did` is accepted
//! (every topic we accept is already per-uuid), one with another `did` is not.
//! Trailing NUL bytes on a device payload are stripped before parsing: the
//! real AC sends its JSON with the C string terminator (T1.9).

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use openssl::asn1::Asn1Time;
use openssl::nid::Nid;
use openssl::pkey::Id;
use openssl::ssl::{SslAcceptor, SslRef};
use openssl::x509::{X509, X509Name, X509NameRef, X509Ref};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use super::pki::{ModulePins, Pki};
use super::tls;
use crate::error::{ControlError, Result};

/// Largest packet accepted (remaining length), well above a `device_packet`.
pub const MAX_PACKET_BYTES: usize = 32 * 1024;
/// Longest topic name or topic filter accepted.
pub const MAX_TOPIC_BYTES: usize = 256;
/// Most topic filters in one SUBSCRIBE or UNSUBSCRIBE; more closes the connection.
pub const MAX_FILTERS_PER_PACKET: usize = 8;
/// Most subscriptions per connection; further filters get SUBACK `0x80`.
pub const MAX_SUBSCRIPTIONS: usize = 16;
/// Most connections served at once (TLS handshakes included).
pub const MAX_CONNECTIONS: usize = 4;
/// Longest hex string [`MqttBroker::send_packet`] accepts.
pub const MAX_HEX_CHARS: usize = 16 * 1024;

/// LG's default provisioning topic (handed out in `completeProvisioning`),
/// followed by the uuid. The module may also publish on the bare
/// `clip/provisioning/devices/<uuid>` form.
pub const PROVISIONING_TOPIC_PREFIX: &str =
    "$aws/rules/clip_provisioning_rule/clip/provisioning/devices/";
/// LG's default message topic: this prefix, the uuid, then `/message`. The
/// module may also publish on the bare `clip/devices/<uuid>/message` form.
pub const MESSAGE_TOPIC_PREFIX: &str = "$aws/rules/clip_http_action_rule/clip/devices/";
/// The topic we publish to, followed by the uuid.
pub const DEVICE_TOPIC_PREFIX: &str = "lime/devices/";

/// Bounded queue of [`DeviceEvent`]s; when full, new events are dropped.
const EVENT_QUEUE: usize = 64;
/// Longest uuid accepted, so every topic stays within [`MAX_TOPIC_BYTES`].
const MAX_UUID_BYTES: usize = 128;
/// Pause after a failed `accept` (for example out of file descriptors).
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);
/// `deployInterval` in `completeProvisioning` (seconds).
const DEPLOY_INTERVAL: u32 = 600;

/// CONNACK return codes (MQTT 3.1.1 §3.2.2.3).
const CONNACK_ACCEPTED: u8 = 0x00;
const CONNACK_BAD_PROTOCOL: u8 = 0x01;
const CONNACK_NOT_AUTHORIZED: u8 = 0x05;
/// SUBACK failure return code.
const SUBACK_FAILURE: u8 = 0x80;

/// Time and connection limits. Production values in [`Limits::PRODUCTION`];
/// tests use shorter ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    max_connections: usize,
    /// From the end of the TLS handshake to a complete CONNECT.
    connect_timeout: Duration,
    /// From a packet's first byte to its last.
    packet_timeout: Duration,
    /// For each write (and flush) to the device.
    write_timeout: Duration,
    /// Bounds for the idle timeout derived from the keep-alive.
    min_idle: Duration,
    max_idle: Duration,
}

impl Limits {
    const PRODUCTION: Self = Self {
        max_connections: MAX_CONNECTIONS,
        connect_timeout: Duration::from_secs(10),
        packet_timeout: Duration::from_secs(10),
        write_timeout: Duration::from_secs(10),
        min_idle: Duration::from_secs(30),
        max_idle: Duration::from_mins(30),
    };

    /// Short timeouts for tests.
    #[cfg(test)]
    pub(crate) const fn short(timeout: Duration) -> Self {
        Self {
            max_connections: MAX_CONNECTIONS,
            connect_timeout: timeout,
            packet_timeout: timeout,
            write_timeout: timeout,
            min_idle: timeout,
            max_idle: timeout,
        }
    }

    /// How long to wait for the next packet: 1.5 × keep-alive, clamped to
    /// `min_idle..=max_idle`; keep-alive 0 (disabled) waits `max_idle`.
    fn idle_timeout(&self, keep_alive_secs: u16) -> Duration {
        if keep_alive_secs == 0 {
            return self.max_idle;
        }
        (Duration::from_secs(u64::from(keep_alive_secs)) * 3 / 2)
            .clamp(self.min_idle, self.max_idle)
    }
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

const CONNECT: u8 = 1;
const CONNACK: u8 = 2;
const PUBLISH: u8 = 3;
const PUBACK: u8 = 4;
const PUBREC: u8 = 5;
const PUBREL: u8 = 6;
const PUBCOMP: u8 = 7;
const SUBSCRIBE: u8 = 8;
const SUBACK: u8 = 9;
const UNSUBSCRIBE: u8 = 10;
const UNSUBACK: u8 = 11;
const PINGREQ: u8 = 12;
const PINGRESP: u8 = 13;
const DISCONNECT: u8 = 14;

/// One raw MQTT packet: the fixed-header byte and the body.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    header: u8,
    body: Vec<u8>,
}

impl Frame {
    fn kind(&self) -> u8 {
        self.header >> 4
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("packet_type", &self.kind())
            .field("len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// Why reading a packet stopped.
#[derive(Debug)]
enum ReadError {
    /// End of stream before a packet started.
    Closed,
    /// No packet started within the idle (or CONNECT) timeout.
    Idle,
    /// A packet started but did not complete within the packet timeout.
    Stalled,
    /// Remaining length over [`MAX_PACKET_BYTES`].
    TooLarge(usize),
    /// Remaining length longer than 4 bytes.
    BadLength,
    Io(std::io::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("connection closed"),
            Self::Idle => f.write_str("no packet within the timeout"),
            Self::Stalled => f.write_str("packet not completed within the timeout"),
            Self::TooLarge(len) => write!(f, "packet of {len} bytes is too large"),
            Self::BadLength => f.write_str("malformed remaining length"),
            Self::Io(e) => write!(f, "read failed: {e}"),
        }
    }
}

/// Reads one packet: its first byte within `first_byte_within`, the rest
/// within `rest_within` of that byte.
async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    first_byte_within: Duration,
    rest_within: Duration,
) -> std::result::Result<Frame, ReadError> {
    let mut first = [0_u8; 1];
    match tokio::time::timeout(first_byte_within, r.read(&mut first)).await {
        Err(_) => return Err(ReadError::Idle),
        Ok(Ok(0)) => return Err(ReadError::Closed),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(ReadError::Io(e)),
    }
    tokio::time::timeout(rest_within, read_rest(r, first[0]))
        .await
        .map_err(|_| ReadError::Stalled)?
}

async fn read_rest<R: AsyncRead + Unpin>(
    r: &mut R,
    header: u8,
) -> std::result::Result<Frame, ReadError> {
    let mut len = 0_usize;
    let mut complete = false;
    for i in 0..4 {
        let byte = r.read_u8().await.map_err(ReadError::Io)?;
        len |= usize::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(ReadError::BadLength);
    }
    if len > MAX_PACKET_BYTES {
        return Err(ReadError::TooLarge(len));
    }
    let mut body = vec![0_u8; len];
    r.read_exact(&mut body).await.map_err(ReadError::Io)?;
    Ok(Frame { header, body })
}

/// A decoded client-to-server packet.
#[derive(PartialEq, Eq)]
enum Packet {
    Connect {
        client_id: String,
        keep_alive: u16,
        /// Protocol level: 4 (MQTT 3.1.1) or 3 (MQTT 3.1). Only logged:
        /// our subset is wire-identical for both.
        level: u8,
    },
    Publish(Publish),
    PubAck,
    Subscribe {
        packet_id: u16,
        filters: Vec<(String, u8)>,
    },
    Unsubscribe {
        packet_id: u16,
        filters: Vec<String>,
    },
    PingReq,
    Disconnect,
}

/// Only the packet kind, so a stray `{:?}` can never print a uuid or payload.
impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Connect { .. } => "Connect",
            Self::Publish(_) => "Publish",
            Self::PubAck => "PubAck",
            Self::Subscribe { .. } => "Subscribe",
            Self::Unsubscribe { .. } => "Unsubscribe",
            Self::PingReq => "PingReq",
            Self::Disconnect => "Disconnect",
        })
    }
}

#[derive(PartialEq, Eq)]
pub(crate) struct Publish {
    topic: String,
    qos: u8,
    packet_id: Option<u16>,
    payload: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum DecodeError {
    /// Neither MQTT 3.1.1 (`"MQTT"`, level 4) nor MQTT 3.1 (`"MQIsdp"`,
    /// level 3): answer CONNACK `0x01`. Carries what the client sent, the
    /// name sanitized (see `printable_name`), for the log.
    UnacceptableProtocol { name: String, level: u8 },
    /// Malformed or unsupported: close the connection.
    Protocol(&'static str),
}

type Decoded<T> = std::result::Result<T, DecodeError>;

/// Reads MQTT primitives from a packet body.
struct Cursor<'a> {
    buf: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Decoded<&'a [u8]> {
        if self.buf.len() < n {
            return Err(DecodeError::Protocol("truncated packet"));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Decoded<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Decoded<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn binary(&mut self) -> Decoded<&'a [u8]> {
        let len = self.u16()?;
        self.take(usize::from(len))
    }

    /// A UTF-8 string without U+0000 (MQTT 3.1.1 §1.5.3).
    fn string(&mut self) -> Decoded<&'a str> {
        let s = std::str::from_utf8(self.binary()?)
            .map_err(|_| DecodeError::Protocol("string is not UTF-8"))?;
        if s.contains('\0') {
            return Err(DecodeError::Protocol("string contains U+0000"));
        }
        Ok(s)
    }

    fn packet_id(&mut self) -> Decoded<u16> {
        match self.u16()? {
            0 => Err(DecodeError::Protocol("packet identifier is 0")),
            id => Ok(id),
        }
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.buf)
    }

    fn end(&self) -> Decoded<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::Protocol("trailing bytes"))
        }
    }
}

/// Decodes a client-to-server packet.
fn decode(frame: &Frame) -> Decoded<Packet> {
    let flags = frame.header & 0x0f;
    let mut c = Cursor { buf: &frame.body };
    let fixed_flags = |want: u8| {
        if flags == want {
            Ok(())
        } else {
            Err(DecodeError::Protocol("invalid fixed-header flags"))
        }
    };
    match frame.kind() {
        CONNECT => {
            fixed_flags(0)?;
            decode_connect(&mut c)
        }
        PUBLISH => decode_publish(flags, &mut c).map(Packet::Publish),
        PUBACK => {
            fixed_flags(0)?;
            c.packet_id()?;
            c.end()?;
            Ok(Packet::PubAck)
        }
        // MQTT 3.1 lets a re-sent SUBSCRIBE/UNSUBSCRIBE set DUP (0b1010).
        SUBSCRIBE => {
            if flags != 0b1010 {
                fixed_flags(0b0010)?;
            }
            decode_subscribe(&mut c)
        }
        UNSUBSCRIBE => {
            if flags != 0b1010 {
                fixed_flags(0b0010)?;
            }
            decode_unsubscribe(&mut c)
        }
        PINGREQ => {
            fixed_flags(0)?;
            c.end()?;
            Ok(Packet::PingReq)
        }
        DISCONNECT => {
            fixed_flags(0)?;
            c.end()?;
            Ok(Packet::Disconnect)
        }
        PUBREC | PUBREL | PUBCOMP => Err(DecodeError::Protocol("QoS 2 is not supported")),
        CONNACK | SUBACK | UNSUBACK | PINGRESP => {
            Err(DecodeError::Protocol("server-to-client packet type"))
        }
        _ => Err(DecodeError::Protocol("reserved packet type")),
    }
}

/// The longest protocol name `printable_name` keeps.
const MAX_LOGGED_PROTOCOL_NAME: usize = 16;

/// A client-sent protocol name made safe to log: printable ASCII only
/// (anything else becomes `?`), at most `MAX_LOGGED_PROTOCOL_NAME` chars.
fn printable_name(name: &str) -> String {
    name.chars()
        .take(MAX_LOGGED_PROTOCOL_NAME)
        .map(|ch| {
            if ch.is_ascii_graphic() || ch == ' ' {
                ch
            } else {
                '?'
            }
        })
        .collect()
}

/// CONNECT: will, username and password are parsed and discarded.
///
/// Accepts MQTT 3.1.1 (`"MQTT"`, level 4) and MQTT 3.1 (`"MQIsdp"`, level
/// 3). 3.1 has the same variable header and flag bits; its 1–23 character
/// client id limit is deliberately not enforced (the uuid is 36), and its
/// CONNACK's first byte is reserved, which our always-0 session-present
/// byte satisfies.
fn decode_connect(c: &mut Cursor<'_>) -> Decoded<Packet> {
    let name = c.string()?;
    let level = c.u8()?;
    if !matches!((name, level), ("MQTT", 4) | ("MQIsdp", 3)) {
        return Err(DecodeError::UnacceptableProtocol {
            name: printable_name(name),
            level,
        });
    }
    let flags = c.u8()?;
    let will = flags & 0x04 != 0;
    let will_qos = (flags >> 3) & 0x03;
    let will_retain = flags & 0x20 != 0;
    let password = flags & 0x40 != 0;
    let username = flags & 0x80 != 0;
    if flags & 0x01 != 0 {
        return Err(DecodeError::Protocol("reserved CONNECT flag set"));
    }
    if will_qos == 3 || (!will && (will_qos != 0 || will_retain)) {
        return Err(DecodeError::Protocol("invalid will flags"));
    }
    if password && !username {
        return Err(DecodeError::Protocol("password without username"));
    }
    let keep_alive = c.u16()?;
    let client_id = c.string()?.to_owned();
    if will {
        c.string()?;
        c.binary()?;
    }
    if username {
        c.string()?;
    }
    if password {
        c.binary()?;
    }
    c.end()?;
    Ok(Packet::Connect {
        client_id,
        keep_alive,
        level,
    })
}

fn decode_publish(flags: u8, c: &mut Cursor<'_>) -> Decoded<Publish> {
    let dup = flags & 0x08 != 0;
    let qos = (flags >> 1) & 0x03;
    match qos {
        3 => return Err(DecodeError::Protocol("invalid QoS 3")),
        2 => return Err(DecodeError::Protocol("QoS 2 is not supported")),
        0 if dup => return Err(DecodeError::Protocol("DUP set on a QoS 0 PUBLISH")),
        _ => {}
    }
    let topic = c.string()?;
    if !valid_topic_name(topic) {
        return Err(DecodeError::Protocol("invalid topic name"));
    }
    let packet_id = if qos == 1 { Some(c.packet_id()?) } else { None };
    Ok(Publish {
        topic: topic.to_owned(),
        qos,
        packet_id,
        payload: c.rest().to_vec(),
    })
}

fn decode_subscribe(c: &mut Cursor<'_>) -> Decoded<Packet> {
    let packet_id = c.packet_id()?;
    let mut filters = Vec::new();
    while !c.buf.is_empty() {
        let filter = c.string()?;
        let qos = c.u8()?;
        if qos > 2 {
            return Err(DecodeError::Protocol("invalid requested QoS"));
        }
        push_filter(&mut filters, (filter.to_owned(), qos), filter)?;
    }
    if filters.is_empty() {
        return Err(DecodeError::Protocol("SUBSCRIBE without filters"));
    }
    Ok(Packet::Subscribe { packet_id, filters })
}

fn decode_unsubscribe(c: &mut Cursor<'_>) -> Decoded<Packet> {
    let packet_id = c.packet_id()?;
    let mut filters = Vec::new();
    while !c.buf.is_empty() {
        let filter = c.string()?;
        push_filter(&mut filters, filter.to_owned(), filter)?;
    }
    if filters.is_empty() {
        return Err(DecodeError::Protocol("UNSUBSCRIBE without filters"));
    }
    Ok(Packet::Unsubscribe { packet_id, filters })
}

fn push_filter<T>(filters: &mut Vec<T>, item: T, filter: &str) -> Decoded<()> {
    if !valid_topic_filter(filter) {
        return Err(DecodeError::Protocol("invalid topic filter"));
    }
    if filters.len() == MAX_FILTERS_PER_PACKET {
        return Err(DecodeError::Protocol("too many topic filters"));
    }
    filters.push(item);
    Ok(())
}

/// A PUBLISH topic: non-empty, at most [`MAX_TOPIC_BYTES`], no wildcards.
fn valid_topic_name(topic: &str) -> bool {
    !topic.is_empty() && topic.len() <= MAX_TOPIC_BYTES && !topic.contains(['+', '#'])
}

/// A topic filter: `+` fills a whole level, `#` fills the whole last level.
fn valid_topic_filter(filter: &str) -> bool {
    if filter.is_empty() || filter.len() > MAX_TOPIC_BYTES {
        return false;
    }
    let mut levels = filter.split('/').peekable();
    while let Some(level) = levels.next() {
        let last = levels.peek().is_none();
        if level.contains('#') && (level != "#" || !last) {
            return false;
        }
        if level.contains('+') && level != "+" {
            return false;
        }
    }
    true
}

/// Whether `filter` matches `topic` (MQTT 3.1.1 §4.7); wildcards at the
/// first level do not match `$`-topics.
fn topic_matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && filter.starts_with(['+', '#']) {
        return false;
    }
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) | (None, None) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

fn encode(header: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(header);
    let mut len = body.len();
    loop {
        // Truncation intended: the low 7 bits of the remaining length.
        #[allow(clippy::cast_possible_truncation)]
        let mut byte = (len & 0x7f) as u8;
        len >>= 7;
        if len > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

/// The protocol of an accepted CONNECT, for the debug log.
fn protocol_label(level: u8) -> &'static str {
    match level {
        3 => "MQTT 3.1",
        4 => "MQTT 3.1.1",
        _ => "MQTT (unknown level)",
    }
}

/// CONNACK with session present = 0: MQTT 3.1.1 has no session to resume
/// here, and in MQTT 3.1 the first byte is reserved and must be 0.
fn encode_connack(code: u8) -> Vec<u8> {
    encode(CONNACK << 4, &[0, code])
}

fn encode_suback(packet_id: u16, codes: &[u8]) -> Vec<u8> {
    let mut body = packet_id.to_be_bytes().to_vec();
    body.extend_from_slice(codes);
    encode(SUBACK << 4, &body)
}

fn encode_unsuback(packet_id: u16) -> Vec<u8> {
    encode(UNSUBACK << 4, &packet_id.to_be_bytes())
}

fn encode_puback(packet_id: u16) -> Vec<u8> {
    encode(PUBACK << 4, &packet_id.to_be_bytes())
}

fn encode_pingresp() -> Vec<u8> {
    encode(PINGRESP << 4, &[])
}

/// A server-to-client PUBLISH. `topic` is one of ours: at most
/// [`MAX_TOPIC_BYTES`] because the uuid is capped at [`MAX_UUID_BYTES`].
fn encode_publish(topic: &str, payload: &[u8], packet_id: Option<u16>) -> Vec<u8> {
    let topic_len = u16::try_from(topic.len()).unwrap_or(u16::MAX);
    let mut body = Vec::with_capacity(topic.len() + payload.len() + 4);
    body.extend_from_slice(&topic_len.to_be_bytes());
    body.extend_from_slice(topic.as_bytes());
    let mut header = PUBLISH << 4;
    if let Some(id) = packet_id {
        header |= 1 << 1;
        body.extend_from_slice(&id.to_be_bytes());
    }
    body.extend_from_slice(payload);
    encode(header, &body)
}

/// A name for logs.
fn packet_name(kind: u8) -> &'static str {
    match kind {
        CONNECT => "CONNECT",
        CONNACK => "CONNACK",
        PUBLISH => "PUBLISH",
        PUBACK => "PUBACK",
        PUBREC => "PUBREC",
        PUBREL => "PUBREL",
        PUBCOMP => "PUBCOMP",
        SUBSCRIBE => "SUBSCRIBE",
        SUBACK => "SUBACK",
        UNSUBSCRIBE => "UNSUBSCRIBE",
        UNSUBACK => "UNSUBACK",
        PINGREQ => "PINGREQ",
        PINGRESP => "PINGRESP",
        DISCONNECT => "DISCONNECT",
        _ => "reserved",
    }
}

/// `topic` with every occurrence of `uuid` replaced by `***`.
fn redact_topic(topic: &str, uuid: &str) -> String {
    topic.replace(uuid, "***")
}

/// Lowercase substrings of JSON keys whose values may identify the device,
/// the home or a secret (KB: `getDeviceInfo` carries uuid, MAC, `bootId`,
/// `eepromChecksum`, `errSSID`). Matching is deliberately broad: masking a
/// harmless value costs nothing, leaking one into the public KB does.
const IDENTIFYING_KEY_PARTS: &[&str] = &[
    "uuid", "deviceid", "ssid", "mac", "serial", "bootid", "eeprom", "token", "otp", "key",
    "password", "secret", "cert", "server", "url", "host",
];

/// Whole key segments (split at `_`, `-` and camelCase) that mark an
/// address: `ip`, `ipAddr`, `ip_address`.
const IDENTIFYING_KEY_SEGMENTS: &[&str] = &["did", "ip", "ipaddr", "ipaddress", "addr"];

/// Replaces identifying values with `***`, recursively: values of
/// identifying keys (see [`is_identifying_key`]) and strings that look
/// like an address or host name (see [`looks_identifying`]).
fn redact_ids(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if is_identifying_key(key) {
                    *v = Value::from("***");
                } else {
                    redact_ids(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_ids),
        Value::String(s) if looks_identifying(s) => *value = Value::from("***"),
        _ => {}
    }
}

fn is_identifying_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    IDENTIFYING_KEY_PARTS
        .iter()
        .any(|part| lower.contains(part))
        || key_segments(key).any(|seg| {
            IDENTIFYING_KEY_SEGMENTS
                .iter()
                .any(|s| seg.eq_ignore_ascii_case(s))
        })
}

/// `macAddr_ip-v4` → `mac`, `Addr`, `ip`, `v4`. Consecutive capitals stay
/// together (`IPAddr` is one segment, matched as `ipaddr`).
fn key_segments(key: &str) -> impl Iterator<Item = &str> {
    let mut segments = Vec::new();
    for part in key.split(['_', '-']) {
        let mut start = 0;
        let bytes = part.as_bytes();
        for i in 1..bytes.len() {
            if bytes[i].is_ascii_uppercase() && bytes[i - 1].is_ascii_lowercase() {
                segments.push(&part[start..i]);
                start = i;
            }
        }
        segments.push(&part[start..]);
    }
    segments.into_iter().filter(|s| !s.is_empty())
}

/// Whether a string holds a URL, an IPv4 address (dotted, or dashed as in
/// `sslip.io` names), an IPv6 address, a MAC address or a DNS name.
fn looks_identifying(s: &str) -> bool {
    if s.contains("://") {
        return true;
    }
    let tokens = |allowed: fn(char) -> bool| {
        s.split(move |c: char| !allowed(c))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
    };
    let ipv4 = |t: &str| t.parse::<std::net::Ipv4Addr>().is_ok();
    tokens(|c| c.is_ascii_digit() || c == '.')
        .into_iter()
        .any(ipv4)
        || tokens(|c| c.is_ascii_digit() || c == '-')
            .into_iter()
            .any(|t| ipv4(&t.replace('-', ".")))
        || tokens(|c| c.is_ascii_hexdigit() || c == ':' || c == '-')
            .into_iter()
            .any(|t| {
                is_mac(t)
                    || (t.matches(':').count() >= 2 && t.parse::<std::net::Ipv6Addr>().is_ok())
            })
        || tokens(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            .into_iter()
            .any(is_dns_name)
}

/// A MAC address: six two-digit hex groups joined by `:` or `-`.
fn is_mac(t: &str) -> bool {
    let groups: Vec<&str> = t.split([':', '-']).collect();
    groups.len() == 6
        && groups
            .iter()
            .all(|g| g.len() == 2 && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// At least two labels and a last label of two or more letters
/// (`ac.example.org`, `192-0-2-5.sslip.io`); `4.5` and `v1.9` are not.
fn is_dns_name(t: &str) -> bool {
    let labels: Vec<&str> = t.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| !l.is_empty())
        && labels
            .last()
            .is_some_and(|l| l.len() >= 2 && l.bytes().all(|b| b.is_ascii_alphabetic()))
}

/// A command name safe to log: `[A-Za-z0-9_]{1,32}`, else `<invalid>`.
fn loggable_cmd(cmd: &str) -> &str {
    let ok = (1..=32).contains(&cmd.len())
        && cmd.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if ok { cmd } else { "<invalid>" }
}

/// Non-empty, even-length ASCII hex of at most [`MAX_HEX_CHARS`].
fn is_hex(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_HEX_CHARS
        && s.len().is_multiple_of(2)
        && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Which provisioning request the module sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvisioningKind {
    /// `preDeploy`: the first connection after setup.
    PreDeploy,
    /// `deploy`: a (re)deploy, for example after a reconnect.
    Deploy,
}

impl ProvisioningKind {
    fn cmd(self) -> &'static str {
        match self {
            Self::PreDeploy => "preDeploy",
            Self::Deploy => "deploy",
        }
    }
}

/// What the device did, in order, from [`MqttBroker::next_event`].
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceEvent {
    /// The device's CONNECT was accepted.
    Connected,
    /// The device acknowledged `completeProvisioning`.
    Provisioned {
        /// The request that was answered.
        kind: ProvisioningKind,
    },
    /// A `device_packet`: the TLV state as hex, as the device sent it.
    DevicePacket {
        /// Even-length ASCII hex.
        hex: String,
    },
    /// The device session ended (closed, failed or taken over).
    Disconnected,
}

/// Hides the packet contents (only its length is shown).
impl fmt::Debug for DeviceEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected => f.write_str("Connected"),
            Self::Provisioned { kind } => {
                f.debug_struct("Provisioned").field("kind", kind).finish()
            }
            Self::DevicePacket { hex } => f
                .debug_struct("DevicePacket")
                .field("hex_len", &hex.len())
                .finish(),
            Self::Disconnected => f.write_str("Disconnected"),
        }
    }
}

/// The fields of a device message we rely on.
#[derive(Deserialize)]
struct DeviceMessage {
    #[serde(default)]
    did: Option<String>,
    cmd: String,
    #[serde(default)]
    data: Value,
}

/// `payload` without its trailing NUL bytes. The real AC sends its JSON as
/// a C string including the terminator (`strlen + 1`, T1.9 capture), which
/// a strict JSON parser rejects. NULs elsewhere are left alone, so they
/// still make the payload unparseable.
fn trim_trailing_nuls(payload: &[u8]) -> &[u8] {
    let end = payload.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &payload[..end]
}

/// Our topics for one uuid.
#[derive(Clone)]
struct Topics {
    /// `lime/devices/<uuid>`: we publish here.
    device: String,
    /// LG's provisioning topic and its bare form.
    provisioning: [String; 2],
    /// LG's message topic and its bare form.
    message: [String; 2],
}

impl Topics {
    fn new(uuid: &str) -> Self {
        Self {
            device: format!("{DEVICE_TOPIC_PREFIX}{uuid}"),
            provisioning: [
                format!("{PROVISIONING_TOPIC_PREFIX}{uuid}"),
                format!("clip/provisioning/devices/{uuid}"),
            ],
            message: [
                format!("{MESSAGE_TOPIC_PREFIX}{uuid}/message"),
                format!("clip/devices/{uuid}/message"),
            ],
        }
    }

    fn accepts(&self, topic: &str) -> bool {
        self.provisioning
            .iter()
            .chain(&self.message)
            .any(|t| t == topic)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `completeProvisioning` with LG's default topic names.
fn complete_provisioning(uuid: &str, topics: &Topics, mid: u64, kind: ProvisioningKind) -> Value {
    json!({
        "did": uuid,
        "mid": mid,
        "cmd": "completeProvisioning",
        "type": 0,
        "data": {
            "result": 0,
            "host": "message",
            "appInfo": {
                "host": "message",
                "publication": {
                    "message": topics.message[0],
                    "provisioning": topics.provisioning[0],
                },
            },
            "provisioningType": kind.cmd(),
            "deployInterval": DEPLOY_INTERVAL,
        },
    })
}

/// A `packet` command carrying TLV hex.
fn packet_command(uuid: &str, mid: u64, hex: &str) -> Value {
    json!({ "did": uuid, "mid": mid, "cmd": "packet", "type": 1, "data": hex })
}

// ---------------------------------------------------------------------------
// Broker
// ---------------------------------------------------------------------------

/// Where the broker listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MqttConfig {
    /// The LAN interface address to listen on. The unspecified address
    /// (`0.0.0.0`, `::`) is refused; loopback is allowed (tests).
    pub bind: IpAddr,
    /// The port: [`MQTT_PORT`](super::cloud::MQTT_PORT) in production, `0`
    /// in tests.
    pub port: u16,
    /// How client certificates not issued by our client CA are accepted;
    /// `None` refuses them all.
    pub pinning: Option<Pinning>,
}

/// Public-key pinning for a module that presents a client certificate we
/// did not issue (D21).
///
/// Such a certificate (any issuer, any common name: the module's real one
/// is AWS IoT-issued with a generic wildcard CN) is accepted only if the
/// CONNECT `clientId` is the session's uuid and its `SubjectPublicKeyInfo`
/// SHA-256 equals the uuid's pin. Without a pin, the first such CONNECT is
/// pinned only inside a pinning window; an existing pin is never replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pinning {
    /// Where the pins are stored (the PKI directory).
    pub pins: ModulePins,
    /// Whether a module without a pin may be pinned now: `ac setup`, and
    /// `ac serve` with `--dns` or `--pin`.
    pub window: bool,
}

impl MqttConfig {
    /// Checks the bind address.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if `bind` is unspecified.
    pub fn validate(&self) -> Result<()> {
        if self.bind.is_unspecified() {
            return Err(ControlError::invalid(
                "the MQTT broker must bind one interface address, not the unspecified address",
            ));
        }
        Ok(())
    }
}

/// The active device connection.
struct Active {
    id: u64,
    outbound: mpsc::Sender<Outbound>,
    /// Dropping it (takeover) ends the connection.
    _kill: oneshot::Sender<()>,
}

/// A message for the device, with a reply once written (or refused).
struct Outbound {
    payload: Vec<u8>,
    done: oneshot::Sender<Result<()>>,
}

/// One write to the device.
struct Write {
    bytes: Vec<u8>,
    done: Option<oneshot::Sender<Result<()>>>,
}

/// Per-connection state shared by its reader and outbound loops.
#[derive(Default)]
struct Connection {
    subscriptions: Vec<(String, u8)>,
    last_packet_id: u16,
    /// The last provisioning request answered on this connection.
    provisioning: Option<ProvisioningKind>,
    /// A provisioning request whose `completeProvisioning` could not be sent
    /// yet (no subscription to `lime/devices/<uuid>`); sent on SUBSCRIBE.
    unsent_reply: Option<ProvisioningKind>,
}

impl Connection {
    /// The highest granted `QoS` among subscriptions matching `topic`.
    fn granted_qos(&self, topic: &str) -> Option<u8> {
        self.subscriptions
            .iter()
            .filter(|(filter, _)| topic_matches(filter, topic))
            .map(|&(_, qos)| qos)
            .max()
    }

    fn next_packet_id(&mut self) -> u16 {
        self.last_packet_id = self.last_packet_id.checked_add(1).unwrap_or(1);
        self.last_packet_id
    }

    /// SUBACK codes: granted `QoS` `min(requested, 1)`, or `0x80` past
    /// [`MAX_SUBSCRIPTIONS`]. A filter already present is replaced.
    fn subscribe(&mut self, filters: Vec<(String, u8)>) -> Vec<u8> {
        filters
            .into_iter()
            .map(|(filter, requested)| {
                let granted = requested.min(1);
                if let Some(entry) = self.subscriptions.iter_mut().find(|(f, _)| *f == filter) {
                    entry.1 = granted;
                    granted
                } else if self.subscriptions.len() < MAX_SUBSCRIPTIONS {
                    self.subscriptions.push((filter, granted));
                    granted
                } else {
                    SUBACK_FAILURE
                }
            })
            .collect()
    }

    fn unsubscribe(&mut self, filters: &[String]) {
        self.subscriptions.retain(|(f, _)| !filters.contains(f));
    }
}

fn lock(conn: &Mutex<Connection>) -> std::sync::MutexGuard<'_, Connection> {
    conn.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One device's broker session: its uuid, topics, limits, event queue and
/// the active connection.
pub struct MqttSession {
    uuid: String,
    topics: Topics,
    limits: Limits,
    pinning: Option<Pinning>,
    /// Makes "read the pin, then write it" one step across connections.
    pin_lock: Mutex<()>,
    events: mpsc::Sender<DeviceEvent>,
    active: Mutex<Option<Active>>,
    next_id: AtomicU64,
}

impl fmt::Debug for MqttSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttSession")
            .field("uuid", &"***")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl MqttSession {
    /// Starts the MQTTS broker for the device `expected_uuid`.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if the config or uuid is invalid or the TLS
    /// profile cannot be built; [`ControlError::Network`] if binding fails.
    pub async fn start(config: &MqttConfig, pki: &Pki, expected_uuid: &str) -> Result<MqttBroker> {
        Self::start_with(config, pki, expected_uuid, Limits::PRODUCTION).await
    }

    pub(crate) async fn start_with(
        config: &MqttConfig,
        pki: &Pki,
        expected_uuid: &str,
        limits: Limits,
    ) -> Result<MqttBroker> {
        config.validate()?;
        let (session, events) = Self::new(expected_uuid, limits, config.pinning.clone())?;
        let acceptor = Arc::new(PeerTls {
            acceptor: tls::mqtt_acceptor(
                pki.server.cert(),
                pki.server.key(),
                pki.client_ca.cert(),
                pki.ca.cert(),
            )?,
            client_ca: pki.client_ca.cert().clone(),
            ours: tls::our_ca_names(&[pki.client_ca.cert(), pki.ca.cert()])?,
        });
        let listener = TcpListener::bind(SocketAddr::new(config.bind, config.port))
            .await
            .map_err(|e| ControlError::network(format!("MQTT bind: {e}")))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| ControlError::network(format!("MQTT bind: {e}")))?;
        tracing::info!(port = local_addr.port(), "mqtt: broker started");
        let task = tokio::spawn(accept_loop(listener, acceptor, Arc::clone(&session)));
        Ok(MqttBroker {
            local_addr,
            task: Some(task),
            session,
            events: tokio::sync::Mutex::new(events),
        })
    }

    fn new(
        uuid: &str,
        limits: Limits,
        pinning: Option<Pinning>,
    ) -> Result<(Arc<Self>, mpsc::Receiver<DeviceEvent>)> {
        let malformed = uuid.is_empty()
            || uuid.len() > MAX_UUID_BYTES
            || uuid.contains(['/', '?', '#', '+', '\0'])
            || uuid.chars().any(char::is_whitespace);
        if malformed {
            return Err(ControlError::invalid("device uuid is empty or malformed"));
        }
        let (events, receiver) = mpsc::channel(EVENT_QUEUE);
        let session = Self {
            uuid: uuid.to_owned(),
            topics: Topics::new(uuid),
            limits,
            pinning,
            pin_lock: Mutex::new(()),
            events,
            active: Mutex::new(None),
            next_id: AtomicU64::new(1),
        };
        Ok((Arc::new(session), receiver))
    }

    fn emit(&self, event: DeviceEvent) {
        if let Err(TrySendError::Full(_)) = self.events.try_send(event) {
            tracing::warn!(
                capacity = EVENT_QUEUE,
                "mqtt: event queue full; event dropped"
            );
        }
    }

    fn active(&self) -> std::sync::MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes this connection the device session, closing any previous one.
    fn register(&self) -> (u64, oneshot::Receiver<()>, mpsc::Receiver<Outbound>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (kill, killed) = oneshot::channel();
        let (outbound, outbound_rx) = mpsc::channel(4);
        let mut active = self.active();
        let previous = active.replace(Active {
            id,
            outbound,
            _kill: kill,
        });
        if previous.is_some() {
            tracing::info!("mqtt: a new connection took over the device session");
            self.emit(DeviceEvent::Disconnected);
        }
        tracing::info!("mqtt: device connected");
        self.emit(DeviceEvent::Connected);
        (id, killed, outbound_rx)
    }

    /// Ends the device session if `id` still owns it.
    fn unregister(&self, id: u64) {
        let mut active = self.active();
        if active.as_ref().is_some_and(|a| a.id == id) {
            *active = None;
            tracing::info!("mqtt: device disconnected");
            self.emit(DeviceEvent::Disconnected);
        }
    }

    /// Serves one connection after its TLS handshake (or a test stream).
    /// `peer` is what the client certificate is.
    async fn serve<S>(&self, mut stream: S, peer: Peer)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        match &peer {
            Peer::ClientCa { .. } => {}
            Peer::Foreign(cert) => {
                // Before any MQTT byte: only the CONNECT's clientId is left
                // to check once this passes.
                if let Err(why) = self.check_pin(cert, false) {
                    self.log_refusal(&why, cert);
                    return;
                }
            }
            Peer::Unusable => {
                tracing::warn!(
                    "mqtt: client certificate refused: issued by our CA but not a valid \
                     client certificate, or unreadable"
                );
                return;
            }
        }
        let Some(keep_alive) = self.accept_connect(&mut stream, &peer).await else {
            return;
        };
        let (id, killed, outbound) = self.register();
        self.run(stream, keep_alive, killed, outbound).await;
        self.unregister(id);
    }

    /// Reads the CONNECT and answers it. Returns the keep-alive if accepted.
    async fn accept_connect<S>(&self, stream: &mut S, peer: &Peer) -> Option<u16>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let limits = self.limits;
        let frame = match read_frame(stream, limits.connect_timeout, limits.packet_timeout).await {
            Ok(frame) => frame,
            Err(e) => {
                tracing::debug!(error = %e, "mqtt: no CONNECT received");
                return None;
            }
        };
        log_received(&frame);
        let code = match decode(&frame) {
            Ok(Packet::Connect {
                client_id,
                keep_alive,
                level,
            }) => {
                tracing::debug!("mqtt: client speaks {}", protocol_label(level));
                if self.authorize(peer, &client_id) {
                    self.write(stream, &encode_connack(CONNACK_ACCEPTED))
                        .await?;
                    return Some(keep_alive);
                }
                CONNACK_NOT_AUTHORIZED
            }
            Err(DecodeError::UnacceptableProtocol { name, level }) => {
                tracing::warn!(
                    protocol = %name,
                    level,
                    "mqtt: CONNECT refused: not MQTT 3.1.1 or 3.1"
                );
                CONNACK_BAD_PROTOCOL
            }
            Ok(_) => {
                log_protocol_error(&frame, "first packet is not CONNECT");
                return None;
            }
            Err(DecodeError::Protocol(reason)) => {
                log_protocol_error(&frame, reason);
                return None;
            }
        };
        self.write(stream, &encode_connack(code)).await;
        None
    }

    /// Whether a CONNECT with `client_id` from `peer` may become the device
    /// session; logs why not. Pins a foreign certificate if this is its
    /// first accepted CONNECT inside a pinning window.
    fn authorize(&self, peer: &Peer, client_id: &str) -> bool {
        match peer {
            Peer::ClientCa { cn } => {
                let ok = cn.as_deref() == Some(self.uuid.as_str()) && client_id == self.uuid;
                if !ok {
                    tracing::warn!(
                        "mqtt: CONNECT refused: certificate CN, clientId and the session's uuid differ"
                    );
                }
                ok
            }
            Peer::Foreign(cert) => {
                let checked = if client_id == self.uuid {
                    self.check_pin(cert, true)
                } else {
                    Err(Refusal::ClientId)
                };
                match checked {
                    Ok(()) => true,
                    Err(why) => {
                        self.log_refusal(&why, cert);
                        false
                    }
                }
            }
            Peer::Unusable => false,
        }
    }

    /// Checks a certificate we did not issue against the uuid's pin (its CN
    /// is not checked, only logged). With `commit`, an unpinned key inside a
    /// pinning window is pinned (and logged at warn); without, it only
    /// passes.
    fn check_pin(&self, cert: &ForeignCert, commit: bool) -> std::result::Result<(), Refusal> {
        let Some(pinning) = &self.pinning else {
            return Err(Refusal::NoPinning);
        };
        let _one_at_a_time = self.pin_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let pinned = pinning
            .pins
            .get(&self.uuid)
            .map_err(|e| Refusal::Store(e.to_string()))?;
        // Both are lowercase hex: `ModulePins` normalises what it stores
        // and reads, and `spki_sha256` comes from `hex::encode`.
        match pinned {
            Some(pin) if pin == cert.spki_sha256 => {
                if commit {
                    tracing::debug!(
                        fingerprint = cert.fingerprint(),
                        "mqtt: module certificate accepted by its pin"
                    );
                }
                Ok(())
            }
            Some(pin) => Err(Refusal::PinMismatch {
                pinned: short_fingerprint(&pin).to_owned(),
            }),
            None if !pinning.window => Err(Refusal::NoPinOutsideWindow),
            None if !commit => Ok(()),
            None => {
                let stored = pinning
                    .pins
                    .pin_first(&self.uuid, &cert.spki_sha256)
                    .map_err(|e| Refusal::Store(e.to_string()))?;
                if stored != cert.spki_sha256 {
                    // Another process pinned a different key meanwhile.
                    return Err(Refusal::PinMismatch {
                        pinned: short_fingerprint(&stored).to_owned(),
                    });
                }
                // Warn: shown at the CLI's default verbosity, so the user
                // sees what was pinned.
                tracing::warn!(
                    fingerprint = cert.fingerprint(),
                    spki_sha256 = %cert.spki_sha256,
                    cn = %self.loggable_cn(cert),
                    issuer = %redact_topic(&cert.issuer, &self.uuid),
                    subject = %redact_topic(&cert.subject, &self.uuid),
                    not_after = %cert.not_after,
                    expired = cert.expired,
                    key = %cert.key,
                    "mqtt: module certificate pinned (SPKI SHA-256); only this key is accepted \
                     from now on"
                );
                Ok(())
            }
        }
    }

    /// The certificate's CN in full, with any occurrence of the uuid as
    /// `***` (the module's real CN is a generic wildcard name).
    fn loggable_cn(&self, cert: &ForeignCert) -> String {
        cert.cn
            .as_deref()
            .map_or_else(|| "<none>".to_owned(), |cn| redact_topic(cn, &self.uuid))
    }

    fn log_refusal(&self, why: &Refusal, cert: &ForeignCert) {
        tracing::warn!(
            reason = %why,
            fingerprint = cert.fingerprint(),
            cn = %self.loggable_cn(cert),
            issuer = %redact_topic(&cert.issuer, &self.uuid),
            "mqtt: module certificate (not from our CA) refused"
        );
    }

    /// One bounded write; `None` if it failed or timed out.
    async fn write<S: AsyncWrite + Unpin>(&self, stream: &mut S, bytes: &[u8]) -> Option<()> {
        let written = tokio::time::timeout(self.limits.write_timeout, async {
            stream.write_all(bytes).await?;
            stream.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => Some(()),
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "mqtt: write failed");
                None
            }
            Err(_) => {
                tracing::warn!("mqtt: write timed out; closing the connection");
                None
            }
        }
    }

    /// Runs a connected session until it ends or is taken over.
    async fn run<S>(
        &self,
        stream: S,
        keep_alive: u16,
        mut killed: oneshot::Receiver<()>,
        outbound: mpsc::Receiver<Outbound>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let (reader, writer) = tokio::io::split(stream);
        let (to_writer, queue) = mpsc::channel(16);
        let conn = Mutex::new(Connection::default());
        let idle = self.limits.idle_timeout(keep_alive);
        tokio::select! {
            () = self.read_loop(reader, &to_writer, &conn, idle) => {}
            () = self.write_loop(writer, queue) => {}
            () = self.outbound_loop(outbound, &to_writer, &conn) => {}
            _ = &mut killed => tracing::debug!("mqtt: connection closed after a takeover"),
        }
    }

    async fn read_loop<R: AsyncRead + Unpin>(
        &self,
        mut reader: R,
        to_writer: &mpsc::Sender<Write>,
        conn: &Mutex<Connection>,
        idle: Duration,
    ) {
        loop {
            let frame = match read_frame(&mut reader, idle, self.limits.packet_timeout).await {
                Ok(frame) => frame,
                Err(ReadError::Closed) => {
                    tracing::debug!("mqtt: device closed the connection without DISCONNECT");
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "mqtt: closing the connection");
                    return;
                }
            };
            log_received(&frame);
            let packet = match decode(&frame) {
                Ok(packet) => packet,
                Err(DecodeError::Protocol(reason)) => return log_protocol_error(&frame, reason),
                Err(DecodeError::UnacceptableProtocol { .. }) => {
                    return log_protocol_error(&frame, "second CONNECT");
                }
            };
            let replies = match packet {
                Packet::Connect { .. } => return log_protocol_error(&frame, "second CONNECT"),
                Packet::Disconnect => return,
                Packet::PubAck => Vec::new(),
                Packet::PingReq => vec![encode_pingresp()],
                Packet::Subscribe { packet_id, filters } => {
                    let codes = lock(conn).subscribe(filters);
                    let mut replies = vec![encode_suback(packet_id, &codes)];
                    replies.extend(self.send_unsent_reply(conn));
                    replies
                }
                Packet::Unsubscribe { packet_id, filters } => {
                    lock(conn).unsubscribe(&filters);
                    vec![encode_unsuback(packet_id)]
                }
                Packet::Publish(publish) => {
                    let mut replies: Vec<_> =
                        publish.packet_id.map(encode_puback).into_iter().collect();
                    replies.extend(self.on_publish(&publish, conn));
                    replies
                }
            };
            for bytes in replies {
                if to_writer.send(Write { bytes, done: None }).await.is_err() {
                    return;
                }
            }
        }
    }

    async fn write_loop<W: AsyncWrite + Unpin>(
        &self,
        mut writer: W,
        mut queue: mpsc::Receiver<Write>,
    ) {
        while let Some(write) = queue.recv().await {
            let written = self.write(&mut writer, &write.bytes).await;
            if let Some(done) = write.done {
                let _ = done.send(written.ok_or_else(|| {
                    ControlError::network("the connection failed while sending to the device")
                }));
            }
            if written.is_none() {
                return;
            }
        }
    }

    async fn outbound_loop(
        &self,
        mut outbound: mpsc::Receiver<Outbound>,
        to_writer: &mpsc::Sender<Write>,
        conn: &Mutex<Connection>,
    ) {
        while let Some(message) = outbound.recv().await {
            match self.to_device(conn, &message.payload) {
                Some(bytes) => {
                    let write = Write {
                        bytes,
                        done: Some(message.done),
                    };
                    if to_writer.send(write).await.is_err() {
                        return;
                    }
                }
                None => {
                    let _ = message.done.send(Err(ControlError::network(
                        "the device is not subscribed to its command topic",
                    )));
                }
            }
        }
    }

    /// A PUBLISH to `lime/devices/<uuid>` at the granted `QoS`, or `None` if
    /// no subscription matches.
    fn to_device(&self, conn: &Mutex<Connection>, payload: &[u8]) -> Option<Vec<u8>> {
        let mut conn = lock(conn);
        let qos = conn.granted_qos(&self.topics.device)?;
        let packet_id = (qos > 0).then(|| conn.next_packet_id());
        drop(conn);
        self.log_payload("to device", &self.topics.device, payload);
        Some(encode_publish(&self.topics.device, payload, packet_id))
    }

    /// Trace-level payload log: JSON only, with the uuid and identifying
    /// fields replaced by `***`; anything else is logged by length only.
    fn log_payload(&self, direction: &'static str, topic: &str, payload: &[u8]) {
        if !tracing::enabled!(tracing::Level::TRACE) {
            return;
        }
        let topic = redact_topic(topic, &self.uuid);
        match serde_json::from_slice::<Value>(payload) {
            Ok(mut value) => {
                redact_ids(&mut value);
                let text = redact_topic(&value.to_string(), &self.uuid);
                tracing::trace!(direction, topic, payload = %text, "mqtt: payload");
            }
            Err(_) => {
                tracing::trace!(
                    direction,
                    topic,
                    len = payload.len(),
                    "mqtt: non-JSON payload"
                );
            }
        }
    }

    /// Handles a device PUBLISH; returns a reply packet, if any.
    fn on_publish(&self, publish: &Publish, conn: &Mutex<Connection>) -> Option<Vec<u8>> {
        let len = publish.payload.len();
        if !self.topics.accepts(&publish.topic) {
            tracing::info!(
                topic = redact_topic(&publish.topic, &self.uuid),
                len,
                "mqtt: PUBLISH on an unknown topic dropped"
            );
            return None;
        }
        let payload = trim_trailing_nuls(&publish.payload);
        self.log_payload("from device", &publish.topic, payload);
        let Ok(message) = serde_json::from_slice::<DeviceMessage>(payload) else {
            tracing::warn!(len, "mqtt: unparseable device message dropped");
            return None;
        };
        let cmd = loggable_cmd(&message.cmd);
        match message.did.as_deref() {
            Some(did) if did != self.uuid => {
                tracing::warn!(
                    cmd,
                    len,
                    "mqtt: device message with a mismatched did dropped"
                );
                return None;
            }
            // The topic already names this uuid (checked above).
            None => tracing::debug!(cmd, len, "mqtt: device message without did accepted"),
            Some(_) => {}
        }
        match message.cmd.as_str() {
            "preDeploy" => self.provision(ProvisioningKind::PreDeploy, conn),
            "deploy" => self.provision(ProvisioningKind::Deploy, conn),
            "completeProvisioning_ack" => {
                // `take` so a re-sent ack (e.g. a QoS 1 DUP) emits nothing.
                let requested = lock(conn).provisioning.take();
                if let Some(kind) = requested {
                    tracing::info!(kind = kind.cmd(), "mqtt: device provisioned");
                    self.emit(DeviceEvent::Provisioned { kind });
                } else {
                    tracing::info!(
                        "mqtt: completeProvisioning_ack without a pending request ignored"
                    );
                }
                None
            }
            "device_packet" => {
                if let Some(hex) = message.data.as_str().filter(|hex| is_hex(hex)) {
                    self.emit(DeviceEvent::DevicePacket {
                        hex: hex.to_owned(),
                    });
                } else {
                    tracing::warn!(len, "mqtt: device_packet without hex data dropped");
                }
                None
            }
            // Not answered: unanswered on the real AC, no ill effect seen (T1.9).
            "req_timesync" => {
                tracing::debug!(cmd, len, "mqtt: req_timesync not answered");
                None
            }
            _ => {
                tracing::info!(cmd, len, "mqtt: unhandled device command");
                None
            }
        }
    }

    /// Answers `preDeploy`/`deploy` with `completeProvisioning`, if the
    /// device is subscribed to `lime/devices/<uuid>`.
    fn provision(&self, kind: ProvisioningKind, conn: &Mutex<Connection>) -> Option<Vec<u8>> {
        lock(conn).provisioning = Some(kind);
        let reply = complete_provisioning(&self.uuid, &self.topics, now_ms(), kind).to_string();
        let packet = self.to_device(conn, reply.as_bytes());
        if packet.is_none() {
            lock(conn).unsent_reply = Some(kind);
            tracing::info!(
                kind = kind.cmd(),
                "mqtt: device is not subscribed to lime/devices/*** yet; \
                 completeProvisioning will be sent after its SUBSCRIBE"
            );
        } else {
            lock(conn).unsent_reply = None;
            tracing::info!(kind = kind.cmd(), "mqtt: completeProvisioning sent");
        }
        packet
    }

    /// After a SUBSCRIBE: the `completeProvisioning` held back by
    /// [`Self::provision`], if the device now receives `lime/devices/<uuid>`.
    fn send_unsent_reply(&self, conn: &Mutex<Connection>) -> Option<Vec<u8>> {
        let kind = lock(conn).unsent_reply?;
        let reply = complete_provisioning(&self.uuid, &self.topics, now_ms(), kind).to_string();
        let packet = self.to_device(conn, reply.as_bytes())?;
        lock(conn).unsent_reply = None;
        tracing::info!(
            kind = kind.cmd(),
            "mqtt: completeProvisioning sent after SUBSCRIBE"
        );
        Some(packet)
    }

    /// See [`MqttBroker::send_packet`].
    async fn send_packet(&self, hex: &str) -> Result<()> {
        if !is_hex(hex) {
            return Err(ControlError::invalid(
                "packet must be non-empty, even-length hex",
            ));
        }
        let payload = packet_command(&self.uuid, now_ms(), hex)
            .to_string()
            .into_bytes();
        let not_connected = || ControlError::network("the device is not connected");
        let outbound = self
            .active()
            .as_ref()
            .map(|a| a.outbound.clone())
            .ok_or_else(not_connected)?;
        let (done, result) = oneshot::channel();
        let wait = self.limits.write_timeout;
        tokio::time::timeout(wait, outbound.send(Outbound { payload, done }))
            .await
            .map_err(|_| ControlError::network("the device connection is busy"))?
            .map_err(|_| not_connected())?;
        tokio::time::timeout(wait * 2, result)
            .await
            .map_err(|_| ControlError::network("sending to the device timed out"))?
            .map_err(|_| {
                ControlError::network("the device disconnected before the packet was sent")
            })?
    }
}

fn log_received(frame: &Frame) {
    tracing::trace!(
        packet = packet_name(frame.kind()),
        len = frame.body.len(),
        "mqtt: packet received"
    );
}

/// Logs a protocol error with the packet type and length only.
fn log_protocol_error(frame: &Frame, reason: &str) {
    tracing::warn!(
        reason,
        packet_type = frame.kind(),
        len = frame.body.len(),
        "mqtt: protocol error; closing the connection"
    );
}

/// What a TLS client's certificate is, decided after the handshake.
pub(crate) enum Peer {
    /// Issued by our client CA for a CSR; `cn` is its single common name.
    ClientCa { cn: Option<String> },
    /// From any other issuer: accepted only by its pin (D21).
    Foreign(Box<ForeignCert>),
    /// From our PKI but not a valid client certificate, or unreadable.
    Unusable,
}

/// The parts of a foreign client certificate the broker checks and logs.
/// Never logged whole: `issuer` and `subject` go through uuid redaction.
pub(crate) struct ForeignCert {
    cn: Option<String>,
    /// Lowercase hex SHA-256 of the DER `SubjectPublicKeyInfo`.
    spki_sha256: String,
    issuer: String,
    subject: String,
    not_after: String,
    /// Outside its validity period now (tolerated, see `tls::foreign_chain_error`).
    expired: bool,
    /// For example `RSA-2048`, `EC-256`.
    key: String,
}

impl ForeignCert {
    pub(crate) fn from_cert(
        cert: &X509Ref,
    ) -> std::result::Result<Self, openssl::error::ErrorStack> {
        let public = cert.public_key()?;
        let spki = public.public_key_to_der()?;
        let kind = match public.id() {
            Id::RSA => "RSA".to_owned(),
            Id::EC => "EC".to_owned(),
            other => format!("key-type-{}", other.as_raw()),
        };
        let now = Asn1Time::days_from_now(0)?;
        let expired = cert.not_after().compare(&now)? == std::cmp::Ordering::Less
            || cert.not_before().compare(&now)? == std::cmp::Ordering::Greater;
        Ok(Self {
            cn: common_name(cert),
            spki_sha256: hex::encode(openssl::sha::sha256(&spki)),
            issuer: render_name(cert.issuer_name()),
            subject: render_name(cert.subject_name()),
            not_after: cert.not_after().to_string(),
            expired,
            key: format!("{kind}-{}", public.bits()),
        })
    }

    fn fingerprint(&self) -> &str {
        short_fingerprint(&self.spki_sha256)
    }
}

/// The first 16 hex characters of a SHA-256 fingerprint.
fn short_fingerprint(hex: &str) -> &str {
    hex.get(..16).unwrap_or(hex)
}

/// Why a foreign client certificate was refused. Never carries the uuid.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    ClientId,
    NoPinning,
    NoPinOutsideWindow,
    PinMismatch { pinned: String },
    Store(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientId => f.write_str("the CONNECT clientId is not the configured ac.uuid"),
            Self::NoPinning => f.write_str("this broker has no pin store"),
            Self::NoPinOutsideWindow => f.write_str(
                "no key pinned for this module yet; run `controlz ac serve --pin` (or --dns) \
                 to pin it on its next connect",
            ),
            Self::PinMismatch { pinned } => write!(
                f,
                "the key differs from the pinned one ({pinned}); if the module really got a \
                 new key, delete the module-pin file in the PKI directory and serve with --pin"
            ),
            Self::Store(e) => write!(f, "pin store: {e}"),
        }
    }
}

/// `CN=…, O=…` with short field names.
fn render_name(name: &X509NameRef) -> String {
    name.entries()
        .map(|entry| {
            let field = entry.object().nid().short_name().unwrap_or("?");
            let value = entry
                .data()
                .to_string()
                .unwrap_or_else(|_| "<binary>".to_owned());
            format!("{field}={value}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Our acceptor plus what classifies its clients' certificates.
struct PeerTls {
    acceptor: SslAcceptor,
    client_ca: X509,
    /// Subject names of our server and client CAs.
    ours: Vec<X509Name>,
}

impl PeerTls {
    /// What the handshake's client certificate is.
    fn classify(&self, ssl: &SslRef) -> Peer {
        let Some(cert) = ssl.peer_certificate() else {
            return Peer::Unusable;
        };
        if tls::chains_to_client_ca(&cert, ssl.peer_cert_chain(), &self.client_ca) {
            return Peer::ClientCa {
                cn: common_name(&cert),
            };
        }
        if tls::issued_by_any(&cert, &self.ours) {
            return Peer::Unusable;
        }
        ForeignCert::from_cert(&cert).map_or(Peer::Unusable, |c| Peer::Foreign(Box::new(c)))
    }
}

/// The single common name of a certificate, if it has exactly one.
fn common_name(cert: &X509Ref) -> Option<String> {
    let mut names = cert.subject_name().entries_by_nid(Nid::COMMONNAME);
    let name = names.next()?;
    if names.next().is_some() {
        return None;
    }
    std::str::from_utf8(name.data().as_slice())
        .ok()
        .map(str::to_owned)
}

/// The running broker for one device. Stop it with
/// [`shutdown`](Self::shutdown); dropping it also stops it.
pub struct MqttBroker {
    local_addr: SocketAddr,
    task: Option<JoinHandle<()>>,
    session: Arc<MqttSession>,
    events: tokio::sync::Mutex<mpsc::Receiver<DeviceEvent>>,
}

impl fmt::Debug for MqttBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttBroker")
            .field("local_addr", &self.local_addr)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl MqttBroker {
    /// The address actually bound (useful with port 0).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The next device event. Events are queued (up to 64; newer ones are
    /// dropped with a warning when full). Never `None` while the broker
    /// exists: it keeps the sending side alive until it is dropped.
    pub async fn next_event(&self) -> Option<DeviceEvent> {
        self.events.lock().await.recv().await
    }

    /// Sends a `packet` command (TLV as hex) to `lime/devices/<uuid>`.
    ///
    /// Succeeds once the PUBLISH is written to the connection; a `QoS` 1
    /// PUBACK is not awaited.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if `hex` is not non-empty, even-length hex
    /// of at most [`MAX_HEX_CHARS`]; [`ControlError::Network`] if no device
    /// is connected, it is not subscribed to its command topic, or the write
    /// fails or times out.
    pub async fn send_packet(&self, hex: &str) -> Result<()> {
        self.session.send_packet(hex).await
    }

    /// Stops accepting, closes every connection and waits until the listener
    /// is closed.
    pub async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            // A cancelled task is the expected outcome.
            let _ = task.await;
        }
        tracing::info!("mqtt: broker stopped");
    }
}

impl Drop for MqttBroker {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Accepts connections until aborted; aborting drops the `JoinSet`, which
/// aborts every connection task.
async fn accept_loop(listener: TcpListener, acceptor: Arc<PeerTls>, session: Arc<MqttSession>) {
    let max = session.limits.max_connections;
    let permits = Arc::new(Semaphore::new(max));
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let stream = match listener.accept().await {
            Ok((stream, _peer)) => stream,
            Err(e) => {
                tracing::warn!(error = %e, "mqtt: accept failed");
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            tracing::warn!(max, "mqtt: too many connections; closing one");
            continue;
        };
        connections.spawn(serve_tcp(
            stream,
            Arc::clone(&acceptor),
            Arc::clone(&session),
            permit,
        ));
    }
}

async fn serve_tcp(
    stream: TcpStream,
    acceptor: Arc<PeerTls>,
    session: Arc<MqttSession>,
    _permit: OwnedSemaphorePermit,
) {
    let tls = match tls::accept(&acceptor.acceptor, stream).await {
        Ok(tls) => tls,
        Err(e) => {
            tracing::debug!(error = %e, "mqtt: TLS handshake failed");
            return;
        }
    };
    let peer = acceptor.classify(tls.ssl());
    session.serve(tls, peer).await;
}

/// The device side of an MQTT session, for tests here and in `controller`.
#[cfg(test)]
pub(crate) mod test_client {
    use super::*;

    /// Upper bound for anything a test waits on.
    pub(crate) const WAIT: Duration = Duration::from_secs(5);

    pub(crate) fn put_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    pub(crate) fn connect_packet(client_id: &str, keep_alive: u16) -> Vec<u8> {
        connect_packet_as("MQTT", 4, client_id, keep_alive)
    }

    pub(crate) fn connect_packet_as(
        name: &str,
        level: u8,
        client_id: &str,
        keep_alive: u16,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        put_str(&mut body, name);
        body.extend_from_slice(&[level, 0x02]);
        body.extend_from_slice(&keep_alive.to_be_bytes());
        put_str(&mut body, client_id);
        encode(CONNECT << 4, &body)
    }

    pub(crate) fn subscribe_body(packet_id: u16, filters: &[(&str, u8)]) -> Vec<u8> {
        let mut body = packet_id.to_be_bytes().to_vec();
        for (filter, qos) in filters {
            put_str(&mut body, filter);
            body.push(*qos);
        }
        body
    }

    pub(crate) fn subscribe_packet(packet_id: u16, filters: &[(&str, u8)]) -> Vec<u8> {
        encode(SUBSCRIBE << 4 | 0b0010, &subscribe_body(packet_id, filters))
    }

    /// A mock device: raw MQTT over any stream.
    pub(crate) struct Client<S> {
        pub(crate) stream: S,
    }

    impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
        pub(crate) async fn send(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).await.unwrap();
            self.stream.flush().await.unwrap();
        }

        pub(crate) async fn recv(&mut self) -> Frame {
            read_frame(&mut self.stream, WAIT, WAIT)
                .await
                .expect("a packet from the broker")
        }

        /// Sends an MQTT 3.1.1 CONNECT and returns the CONNACK return code.
        pub(crate) async fn connect(&mut self, client_id: &str) -> u8 {
            self.connect_as("MQTT", 4, client_id).await
        }

        /// Sends CONNECT with this protocol name and level and returns the
        /// CONNACK return code; session present must be 0.
        pub(crate) async fn connect_as(&mut self, name: &str, level: u8, client_id: &str) -> u8 {
            self.send(&connect_packet_as(name, level, client_id, 60))
                .await;
            let connack = self.recv().await;
            assert_eq!(connack.header, CONNACK << 4);
            assert_eq!(connack.body[0], 0, "session present is 0");
            connack.body[1]
        }

        /// Subscribes to one filter and returns its SUBACK code.
        pub(crate) async fn subscribe(&mut self, filter: &str, qos: u8) -> u8 {
            self.send(&subscribe_packet(7, &[(filter, qos)])).await;
            let suback = self.recv().await;
            assert_eq!(suback.header, SUBACK << 4);
            assert_eq!(&suback.body[..2], &[0, 7]);
            suback.body[2]
        }

        pub(crate) async fn publish(&mut self, topic: &str, payload: &[u8]) {
            self.send(&encode_publish(topic, payload, None)).await;
        }

        pub(crate) async fn recv_publish(&mut self) -> Publish {
            let frame = self.recv().await;
            match decode(&frame) {
                Ok(Packet::Publish(p)) => p,
                other => panic!("expected PUBLISH, got {other:?} ({frame:?})"),
            }
        }

        /// PINGREQ → PINGRESP: proves the broker sent nothing in between.
        pub(crate) async fn ping(&mut self) {
            self.send(&[PINGREQ << 4, 0]).await;
            let reply = self.recv().await;
            assert_eq!(
                (reply.header, reply.body.len()),
                (PINGRESP << 4, 0),
                "{reply:?}"
            );
        }

        pub(crate) async fn expect_closed(&mut self) {
            let mut buf = [0_u8; 256];
            loop {
                let read = tokio::time::timeout(WAIT, self.stream.read(&mut buf))
                    .await
                    .expect("the broker must close the connection");
                if matches!(read, Ok(0) | Err(_)) {
                    return;
                }
            }
        }

        /// The next PUBLISH from the broker as (topic, JSON payload); a
        /// `QoS` 1 PUBLISH is acknowledged.
        pub(crate) async fn recv_json(&mut self) -> (String, Value) {
            let publish = self.recv_publish().await;
            if let Some(id) = publish.packet_id {
                self.send(&encode_puback(id)).await;
            }
            let value = serde_json::from_slice(&publish.payload).unwrap();
            (publish.topic, value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_client::{
        Client, WAIT, connect_packet, put_str, subscribe_body, subscribe_packet,
    };
    use super::*;
    use crate::ac::pki::test_fixture::{
        FIXTURE, FOREIGN_CA, HOSTNAME, csr_pem, mismatched_rsa_key, p256_key,
    };
    use crate::ac::pki::{Ca, Identity};
    use crate::secret::Secret;
    use openssl::ssl::{SslConnector, SslMethod, SslVersion};
    use std::io;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::pin::Pin;
    use std::time::Instant;
    use tokio::io::DuplexStream;
    use tokio_openssl::SslStream;

    const UUID: &str = "00000000-fake-0000-0000-000000000000";
    const OTHER: &str = "11111111-fake-1111-1111-111111111111";
    /// The timeout under test in `Limits::short`.
    const SHORT: Duration = Duration::from_millis(300);
    /// The `device_packet` fixture's data: a full state report, in the
    /// uppercase hex the real AC sends.
    const PACKET_HEX: &str = "0000040000008702042A7A7E407DC17E847F50317F902EC840C880C8C08340838083C0868086C0870087C18F80894088408A0C8A503C8A90148CA0012C8CD01EACD0C8CA00D540D580C900CAD050CB1064CB40CB87CBCCCC00CC90788B40BF6001F4BFC0BFA001F4C000BE501EBE86CC50781B01BED079C340C0C0C380CCC0CD00CD40900042AC";

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/ac_mqtt/",
                $name,
                ".json"
            ))
            .as_slice()
        };
    }

    fn prov_topic() -> String {
        format!("{PROVISIONING_TOPIC_PREFIX}{UUID}")
    }

    fn bare_prov_topic() -> String {
        format!("clip/provisioning/devices/{UUID}")
    }

    fn msg_topic() -> String {
        format!("{MESSAGE_TOPIC_PREFIX}{UUID}/message")
    }

    fn bare_msg_topic() -> String {
        format!("clip/devices/{UUID}/message")
    }

    fn device_topic() -> String {
        format!("lime/devices/{UUID}")
    }

    // ---- Client-side encoding ----

    fn connect_body(name: &str, level: u8, flags: u8, client_id: &str) -> Vec<u8> {
        let mut body = Vec::new();
        put_str(&mut body, name);
        body.push(level);
        body.push(flags);
        body.extend_from_slice(&60_u16.to_be_bytes());
        put_str(&mut body, client_id);
        body
    }

    fn unsubscribe_packet(packet_id: u16, filter: &str) -> Vec<u8> {
        let mut body = packet_id.to_be_bytes().to_vec();
        put_str(&mut body, filter);
        encode(UNSUBSCRIBE << 4 | 0b0010, &body)
    }

    /// A device PUBLISH at `QoS` 1 with `packet_id`.
    fn publish_qos1(topic: &str, payload: &[u8], packet_id: u16) -> Vec<u8> {
        encode_publish(topic, payload, Some(packet_id))
    }

    fn message(did: &str, cmd: &str, data: &Value) -> Vec<u8> {
        json!({"did": did, "mid": 1, "cmd": cmd, "type": 0, "data": data})
            .to_string()
            .into_bytes()
    }

    /// Parses one complete frame from `bytes` (header, varint, body).
    fn frame(bytes: &[u8]) -> Frame {
        let mut len = 0_usize;
        let mut i = 1;
        loop {
            len |= usize::from(bytes[i] & 0x7f) << (7 * (i - 1));
            i += 1;
            if bytes[i - 1] & 0x80 == 0 {
                break;
            }
        }
        assert_eq!(bytes.len(), i + len, "one complete frame");
        Frame {
            header: bytes[0],
            body: bytes[i..].to_vec(),
        }
    }

    // ---- Mock device ----

    impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
        /// The full provisioning exchange for `kind` on `topic`.
        async fn provision(&mut self, topic: &str, kind: ProvisioningKind) {
            let request = match kind {
                ProvisioningKind::PreDeploy => fixture!("predeploy"),
                ProvisioningKind::Deploy => fixture!("deploy"),
            };
            self.publish(topic, request).await;
            let reply = self.recv_publish().await;
            assert_eq!(reply.topic, device_topic());
            assert_eq!(reply.qos, 1);
            let body: Value = serde_json::from_slice(&reply.payload).unwrap();
            assert_eq!(body["cmd"], "completeProvisioning");
            assert_eq!(body["did"], UUID);
            assert_eq!(body["type"], 0);
            assert_eq!(body["data"]["result"], 0);
            assert_eq!(body["data"]["provisioningType"], kind.cmd());
            assert_eq!(
                body["data"]["appInfo"]["publication"],
                json!({"message": msg_topic(), "provisioning": prov_topic()})
            );
            self.send(&encode_puback(reply.packet_id.unwrap())).await;
            self.publish(topic, fixture!("complete_provisioning_ack"))
                .await;
        }
    }

    // ---- In-memory sessions ----

    fn session(limits: Limits) -> (Arc<MqttSession>, mpsc::Receiver<DeviceEvent>) {
        MqttSession::new(UUID, limits, None).unwrap()
    }

    fn attach(
        session: &Arc<MqttSession>,
        peer_cn: Option<&str>,
    ) -> (Client<DuplexStream>, JoinHandle<()>) {
        attach_with_buffer(session, peer_cn, 64 * 1024)
    }

    fn attach_with_buffer(
        session: &Arc<MqttSession>,
        peer_cn: Option<&str>,
        buffer: usize,
    ) -> (Client<DuplexStream>, JoinHandle<()>) {
        let (client, server) = tokio::io::duplex(buffer);
        let session = Arc::clone(session);
        let peer = Peer::ClientCa {
            cn: peer_cn.map(str::to_owned),
        };
        let task = tokio::spawn(async move { session.serve(server, peer).await });
        (Client { stream: client }, task)
    }

    /// A connected and subscribed in-memory device.
    async fn connected(session: &Arc<MqttSession>) -> Client<DuplexStream> {
        let (mut client, _task) = attach(session, Some(UUID));
        assert_eq!(client.connect(UUID).await, CONNACK_ACCEPTED);
        assert_eq!(client.subscribe(&device_topic(), 1).await, 1);
        client
    }

    async fn event(events: &mut mpsc::Receiver<DeviceEvent>) -> DeviceEvent {
        tokio::time::timeout(WAIT, events.recv())
            .await
            .expect("an event")
            .expect("event queue open")
    }

    fn no_event(events: &mut mpsc::Receiver<DeviceEvent>) {
        assert!(events.try_recv().is_err(), "no event expected");
    }

    // ---- Logs ----

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

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Captures logs on this thread (`#[tokio::test]` runs every task on it).
    fn capture() -> (Buffer, tracing::subscriber::DefaultGuard) {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        (buffer, tracing::subscriber::set_default(subscriber))
    }

    // ---- TLS over TCP ----

    fn config() -> MqttConfig {
        MqttConfig {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            pinning: None,
        }
    }

    async fn start() -> MqttBroker {
        MqttSession::start(&config(), &FIXTURE, UUID).await.unwrap()
    }

    /// An EC client certificate for `cn` signed by `ca` (clientAuth EKU).
    fn client_identity(ca: &Ca, cn: &str) -> Identity {
        let key = p256_key();
        let pem = ca.sign_csr(&csr_pem(&key, &[cn]), cn, 30).unwrap();
        let key_pem =
            Secret::new(String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap());
        Identity::from_pem(&pem, &key_pem).unwrap()
    }

    /// A module-like TLS client that trusts our server CA.
    async fn tls_connect(
        addr: SocketAddr,
        identity: Option<&Identity>,
    ) -> std::result::Result<Client<SslStream<TcpStream>>, String> {
        let mut b = SslConnector::builder(SslMethod::tls_client()).unwrap();
        b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        b.set_cipher_list("ECDHE-RSA-AES256-SHA@SECLEVEL=0")
            .unwrap();
        b.cert_store_mut()
            .add_cert(FIXTURE.ca.cert().clone())
            .unwrap();
        if let Some(id) = identity {
            b.set_certificate(id.cert()).unwrap();
            b.set_private_key(id.key()).unwrap();
        }
        let ssl = b.build().configure().unwrap().into_ssl(HOSTNAME).unwrap();
        let tcp = TcpStream::connect(addr).await.unwrap();
        let mut tls = SslStream::new(ssl, tcp).unwrap();
        Pin::new(&mut tls)
            .connect()
            .await
            .map_err(|e| e.to_string())?;
        Ok(Client { stream: tls })
    }

    async fn next(broker: &MqttBroker) -> DeviceEvent {
        tokio::time::timeout(WAIT, broker.next_event())
            .await
            .expect("an event")
            .expect("event queue open")
    }

    async fn tls_device(broker: &MqttBroker, id: &Identity) -> Client<SslStream<TcpStream>> {
        tls_device_as(broker, id, "MQTT", 4).await
    }

    /// Like `tls_device`, speaking this protocol name and level.
    async fn tls_device_as(
        broker: &MqttBroker,
        id: &Identity,
        name: &str,
        level: u8,
    ) -> Client<SslStream<TcpStream>> {
        let mut client = tls_connect(broker.local_addr(), Some(id)).await.unwrap();
        assert_eq!(client.connect_as(name, level, UUID).await, CONNACK_ACCEPTED);
        assert_eq!(next(broker).await, DeviceEvent::Connected);
        assert_eq!(client.subscribe(&device_topic(), 1).await, 1);
        client
    }

    // ---- Acceptance: provisioning, redeploy, reconnect, takeover ----

    #[tokio::test]
    async fn mock_device_provisions_and_redeploys() {
        let broker = start().await;
        let addr = broker.local_addr();
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        let id = client_identity(&FIXTURE.client_ca, UUID);

        // First connection: preDeploy on LG's topic.
        let mut device = tls_device(&broker, &id).await;
        device
            .provision(&prov_topic(), ProvisioningKind::PreDeploy)
            .await;
        assert_eq!(
            next(&broker).await,
            DeviceEvent::Provisioned {
                kind: ProvisioningKind::PreDeploy
            }
        );
        device
            .publish(&msg_topic(), fixture!("device_packet"))
            .await;
        assert_eq!(
            next(&broker).await,
            DeviceEvent::DevicePacket {
                hex: PACKET_HEX.to_owned()
            }
        );
        // req_timesync gets no reply.
        device.publish(&msg_topic(), fixture!("req_timesync")).await;
        device.ping().await;
        device.send(&[DISCONNECT << 4, 0]).await;
        device.expect_closed().await;
        assert_eq!(next(&broker).await, DeviceEvent::Disconnected);

        // New TLS connection: deploy on the bare topic form.
        let mut device = tls_device(&broker, &id).await;
        device
            .provision(&bare_prov_topic(), ProvisioningKind::Deploy)
            .await;
        assert_eq!(
            next(&broker).await,
            DeviceEvent::Provisioned {
                kind: ProvisioningKind::Deploy
            }
        );

        // A command reaches the device.
        broker.send_packet("0a0B").await.unwrap();
        let command = device.recv_publish().await;
        assert_eq!(command.topic, device_topic());
        assert_eq!(command.qos, 1);
        let body: Value = serde_json::from_slice(&command.payload).unwrap();
        assert!(body["mid"].as_u64().unwrap() > 1_600_000_000_000);
        assert_eq!(
            body,
            json!({"did": UUID, "mid": body["mid"], "cmd": "packet", "type": 1, "data": "0a0B"})
        );

        broker.shutdown().await;
        assert!(TcpStream::connect(addr).await.is_err(), "listener closed");
    }

    #[tokio::test]
    async fn a_repeated_ack_emits_no_second_provisioned_event() {
        let (logs, _guard) = capture();
        let (s, mut events) = session(Limits::PRODUCTION);
        let mut device = connected(&s).await;
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        device
            .provision(&prov_topic(), ProvisioningKind::PreDeploy)
            .await;
        assert_eq!(
            event(&mut events).await,
            DeviceEvent::Provisioned {
                kind: ProvisioningKind::PreDeploy
            }
        );
        // The same ack again, as a QoS 1 DUP redelivery would bring it.
        device
            .publish(&prov_topic(), fixture!("complete_provisioning_ack"))
            .await;
        device.ping().await;
        no_event(&mut events);
        let text = logs.text();
        assert_eq!(
            text.matches("mqtt: device provisioned").count(),
            1,
            "{text}"
        );
        assert!(
            text.contains("mqtt: completeProvisioning_ack without a pending request ignored"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn reconnect_after_a_dropped_connection() {
        let broker = start().await;
        let id = client_identity(&FIXTURE.client_ca, UUID);
        let mut device = tls_device(&broker, &id).await;
        device
            .provision(&prov_topic(), ProvisioningKind::PreDeploy)
            .await;
        assert!(matches!(
            next(&broker).await,
            DeviceEvent::Provisioned { .. }
        ));

        // Abrupt drop: no DISCONNECT, RST instead of FIN.
        device.stream.get_ref().set_zero_linger().unwrap();
        drop(device);
        assert_eq!(next(&broker).await, DeviceEvent::Disconnected);
        assert!(matches!(
            broker.send_packet("00").await,
            Err(ControlError::Network { .. })
        ));

        let mut device = tls_device(&broker, &id).await;
        device
            .provision(&prov_topic(), ProvisioningKind::Deploy)
            .await;
        assert_eq!(
            next(&broker).await,
            DeviceEvent::Provisioned {
                kind: ProvisioningKind::Deploy
            }
        );
        broker.send_packet("00ff").await.unwrap();
        let command = device.recv_publish().await;
        let body: Value = serde_json::from_slice(&command.payload).unwrap();
        assert_eq!(body["data"], "00ff");
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn a_new_connection_takes_over() {
        let (s, mut events) = session(Limits::PRODUCTION);
        let mut first = connected(&s).await;
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        let mut second = connected(&s).await;
        assert_eq!(event(&mut events).await, DeviceEvent::Disconnected);
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        first.expect_closed().await;
        no_event(&mut events);

        s.send_packet("abcd").await.unwrap();
        let command = second.recv_publish().await;
        let body: Value = serde_json::from_slice(&command.payload).unwrap();
        assert_eq!(body["data"], "abcd");
        second.send(&[DISCONNECT << 4, 0]).await;
        second.expect_closed().await;
        assert_eq!(event(&mut events).await, DeviceEvent::Disconnected);
    }

    // ---- Acceptance: hardening ----

    #[tokio::test]
    async fn clients_without_a_client_ca_certificate_fail_the_handshake() {
        let (logs, _guard) = capture();
        let broker = start().await;
        let addr = broker.local_addr();
        let wrong_ca = client_identity(&FIXTURE.ca, UUID);
        let server_only = FIXTURE.client_ca.issue_server(UUID).unwrap();
        for (identity, what) in [
            (None, "no certificate"),
            (Some(&wrong_ca), "certificate from the server CA"),
            (Some(&server_only), "certificate without clientAuth"),
        ] {
            assert!(
                tls_connect(addr, identity).await.is_err(),
                "{what}: handshake must fail"
            );
        }
        let deadline = Instant::now() + WAIT;
        while logs.text().matches("mqtt: TLS handshake failed").count() < 3 {
            assert!(Instant::now() < deadline, "{}", logs.text());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let text = logs.text();
        assert!(!text.contains("mqtt: packet received"), "{text}");
        assert!(!text.contains("mqtt: device connected"), "{text}");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), broker.next_event())
                .await
                .is_err(),
            "no device event"
        );
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn connect_needs_cert_cn_client_id_and_uuid_to_match() {
        let broker = start().await;
        let addr = broker.local_addr();
        let ours = client_identity(&FIXTURE.client_ca, UUID);
        let other = client_identity(&FIXTURE.client_ca, OTHER);
        for (identity, client_id) in [(&other, OTHER), (&other, UUID), (&ours, OTHER)] {
            let mut client = tls_connect(addr, Some(identity)).await.unwrap();
            assert_eq!(client.connect(client_id).await, CONNACK_NOT_AUTHORIZED);
            client.expect_closed().await;
        }
        let mut client = tls_connect(addr, Some(&ours)).await.unwrap();
        assert_eq!(client.connect(UUID).await, CONNACK_ACCEPTED);
        assert_eq!(
            next(&broker).await,
            DeviceEvent::Connected,
            "only the match connects"
        );
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn connect_without_a_peer_name_is_refused() {
        let (s, mut events) = session(Limits::PRODUCTION);
        let (mut client, _task) = attach(&s, None);
        assert_eq!(client.connect(UUID).await, CONNACK_NOT_AUTHORIZED);
        client.expect_closed().await;
        no_event(&mut events);
    }

    // ---- Acceptance: foreign client certificates, pinned (D21) ----

    /// A fresh, empty pin directory for one test.
    fn pin_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("controlz-mqtt-pins-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    async fn start_pinning(dir: &std::path::Path, window: bool) -> MqttBroker {
        let config = MqttConfig {
            pinning: Some(Pinning {
                pins: ModulePins::new(dir),
                window,
            }),
            ..config()
        };
        MqttSession::start(&config, &FIXTURE, UUID).await.unwrap()
    }

    fn pin_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .map_or_default(|entries| entries.map(|e| e.unwrap().path()).collect())
    }

    /// Waits until `needle` shows up in the logs.
    async fn logged(logs: &Buffer, needle: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let text = logs.text();
            if text.contains(needle) {
                return text;
            }
            assert!(Instant::now() < deadline, "no {needle:?} in: {text}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn spki_sha256(id: &Identity) -> String {
        let der = id.cert().public_key().unwrap().public_key_to_der().unwrap();
        hex::encode(openssl::sha::sha256(&der))
    }

    fn spki_fingerprint(id: &Identity) -> String {
        spki_sha256(id)[..16].to_owned()
    }

    #[tokio::test]
    async fn our_ca_client_still_connects_and_is_never_pinned() {
        let dir = pin_dir("ours");
        let broker = start_pinning(&dir, true).await;
        let id = client_identity(&FIXTURE.client_ca, UUID);
        let _device = tls_device(&broker, &id).await;
        assert_eq!(
            pin_files(&dir),
            Vec::<std::path::PathBuf>::new(),
            "the CSR path pins nothing"
        );
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn foreign_cert_is_pinned_in_a_window_and_only_that_key_is_accepted_later() {
        let (logs, _guard) = capture();
        let dir = pin_dir("pinned");
        let lg = client_identity(&FOREIGN_CA, UUID);
        let fingerprint = spki_fingerprint(&lg);

        let broker = start_pinning(&dir, true).await;
        let mut device = tls_device(&broker, &lg).await;
        device
            .provision(&prov_topic(), ProvisioningKind::Deploy)
            .await;
        assert_eq!(
            next(&broker).await,
            DeviceEvent::Provisioned {
                kind: ProvisioningKind::Deploy
            }
        );
        let text = logged(&logs, "mqtt: module certificate pinned").await;
        let event = text
            .lines()
            .find(|l| l.contains("mqtt: module certificate pinned"))
            .unwrap();
        assert!(
            event.contains(" WARN "),
            "warn level, shown by default: {event}"
        );
        assert!(
            event.contains(&format!("fingerprint=\"{fingerprint}\"")),
            "{event}"
        );
        assert!(
            event.contains(&format!("spki_sha256={}", spki_sha256(&lg))),
            "full SPKI SHA-256: {event}"
        );
        assert!(event.contains("cn=***"), "{event}");
        assert!(text.contains("issuer=CN=Fake LG test CA"), "{text}");
        assert!(text.contains("subject=CN=***"), "{text}");
        assert!(text.contains("key=EC-256"), "{text}");
        assert!(text.contains("expired=false"), "{text}");
        assert!(text.contains("not_after="), "{text}");
        assert!(!text.contains(UUID), "no uuid in the logs: {text}");
        let files = pin_files(&dir);
        assert_eq!(files.len(), 1, "{files:?}");
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&files[0]).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
        drop(device);
        broker.shutdown().await;

        // After a restart outside any window: the pinned key still connects.
        let broker = start_pinning(&dir, false).await;
        let device = tls_device(&broker, &lg).await;
        logged(&logs, "mqtt: module certificate accepted by its pin").await;

        // Same uuid and issuer, other key: refused before any MQTT byte, even
        // in a window, and the pin stays.
        drop(device);
        broker.shutdown().await;
        let broker = start_pinning(&dir, true).await;
        let impostor = client_identity(&FOREIGN_CA, UUID);
        let mut client = tls_connect(broker.local_addr(), Some(&impostor))
            .await
            .unwrap();
        client.expect_closed().await;
        let text = logged(&logs, "differs from the pinned one").await;
        assert!(text.contains(&format!("({fingerprint})")), "{text}");
        assert!(
            text.contains(&format!("fingerprint=\"{}\"", spki_fingerprint(&impostor))),
            "{text}"
        );
        assert_eq!(
            ModulePins::new(&dir).get(UUID).unwrap().unwrap()[..16],
            fingerprint
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), broker.next_event())
                .await
                .is_err(),
            "no device event"
        );
        assert!(!logs.text().contains(UUID));
        broker.shutdown().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn foreign_cert_with_another_client_id_is_refused_and_not_pinned() {
        let (logs, _guard) = capture();
        let dir = pin_dir("clientid");
        let broker = start_pinning(&dir, true).await;
        let lg = client_identity(&FOREIGN_CA, UUID);
        let mut client = tls_connect(broker.local_addr(), Some(&lg)).await.unwrap();
        assert_eq!(client.connect(OTHER).await, CONNACK_NOT_AUTHORIZED);
        client.expect_closed().await;
        logged(&logs, "clientId is not the configured ac.uuid").await;
        assert_eq!(
            pin_files(&dir),
            Vec::<std::path::PathBuf>::new(),
            "nothing pinned"
        );
        assert!(!logs.text().contains(UUID) && !logs.text().contains(OTHER));
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn foreign_cert_with_a_wildcard_cn_is_pinned_and_its_cn_logged() {
        // Like the real module: an AWS IoT-style certificate whose CN is a
        // generic wildcard name, not the uuid. Only the clientId counts.
        // It speaks MQTT 3.1 here: pinning does not depend on the level.
        let (logs, _guard) = capture();
        let dir = pin_dir("cn");
        let broker = start_pinning(&dir, true).await;
        let lg = client_identity(&FOREIGN_CA, "*.example.com");
        let device = tls_device_as(&broker, &lg, "MQIsdp", 3).await;
        let text = logged(&logs, "mqtt: module certificate pinned").await;
        assert!(text.contains("cn=*.example.com"), "{text}");
        assert!(text.contains("subject=CN=*.example.com"), "{text}");
        assert!(!text.contains("refused"), "{text}");
        assert_eq!(
            ModulePins::new(&dir).get(UUID).unwrap(),
            Some(spki_sha256(&lg))
        );
        drop(device);
        broker.shutdown().await;

        // Outside a window, the pinned key with that CN is still accepted.
        let broker = start_pinning(&dir, false).await;
        let _device = tls_device(&broker, &lg).await;
        logged(&logs, "mqtt: module certificate accepted by its pin").await;
        assert!(!logs.text().contains(UUID), "no uuid in the logs");
        broker.shutdown().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_cn_containing_the_uuid_is_logged_redacted() {
        let (logs, _guard) = capture();
        let dir = pin_dir("cnuuid");
        let broker = start_pinning(&dir, false).await;
        let lg = client_identity(&FOREIGN_CA, &format!("lge-{UUID}"));
        let mut client = tls_connect(broker.local_addr(), Some(&lg)).await.unwrap();
        client.expect_closed().await;
        let text = logged(&logs, "no key pinned for this module yet").await;
        assert!(text.contains("cn=lge-***"), "{text}");
        assert!(!text.contains(UUID), "{text}");
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn foreign_cert_without_pin_outside_a_window_is_refused() {
        let (logs, _guard) = capture();
        let dir = pin_dir("nowindow");
        let broker = start_pinning(&dir, false).await;
        let lg = client_identity(&FOREIGN_CA, UUID);
        let mut client = tls_connect(broker.local_addr(), Some(&lg)).await.unwrap();
        client.expect_closed().await;
        let text = logged(&logs, "no key pinned for this module yet").await;
        assert!(text.contains("controlz ac serve --pin"), "{text}");
        assert!(!text.contains("mqtt: packet received"), "{text}");
        assert_eq!(pin_files(&dir), Vec::<std::path::PathBuf>::new());
        // Without a certificate at all, the handshake itself fails.
        assert!(tls_connect(broker.local_addr(), None).await.is_err());
        broker.shutdown().await;

        // And a broker without a pin store refuses foreign certificates.
        let broker = start().await;
        let mut client = tls_connect(broker.local_addr(), Some(&lg)).await.unwrap();
        client.expect_closed().await;
        logged(&logs, "this broker has no pin store").await;
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn foreign_cert_without_its_private_key_fails_the_handshake() {
        let dir = pin_dir("nokey");
        let broker = start_pinning(&dir, true).await;
        let rsa =
            openssl::pkey::PKey::from_rsa(openssl::rsa::Rsa::generate(2048).unwrap()).unwrap();
        let pem = FOREIGN_CA
            .sign_csr(&csr_pem(&rsa, &[UUID]), UUID, 30)
            .unwrap();
        let wrong = mismatched_rsa_key(&rsa);
        let key_pem =
            Secret::new(String::from_utf8(wrong.private_key_to_pem_pkcs8().unwrap()).unwrap());
        let id = Identity::from_pem(&pem, &key_pem).unwrap();
        assert!(tls_connect(broker.local_addr(), Some(&id)).await.is_err());
        assert_eq!(
            pin_files(&dir),
            Vec::<std::path::PathBuf>::new(),
            "nothing pinned"
        );
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn silent_or_partial_connect_times_out() {
        let (s, mut events) = session(Limits::short(SHORT));
        for bytes in [&[][..], &connect_packet(UUID, 60)[..3]] {
            let (mut client, _task) = attach(&s, Some(UUID));
            client.send(bytes).await;
            let started = Instant::now();
            client.expect_closed().await;
            assert!(started.elapsed() >= SHORT / 2, "closed by the timeout");
        }
        no_event(&mut events);
    }

    #[tokio::test]
    async fn stalled_packet_times_out() {
        let (s, mut events) = session(Limits::short(SHORT));
        let (mut client, _task) = attach(&s, Some(UUID));
        assert_eq!(client.connect(UUID).await, CONNACK_ACCEPTED);
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        // Half a SUBSCRIBE, then silence.
        let packet = subscribe_packet(1, &[("a/b", 0)]);
        client.send(&packet[..4]).await;
        client.expect_closed().await;
        assert_eq!(event(&mut events).await, DeviceEvent::Disconnected);
    }

    #[tokio::test]
    async fn idle_connection_times_out() {
        let (s, mut events) = session(Limits::short(SHORT));
        let (mut client, _task) = attach(&s, Some(UUID));
        client.send(&connect_packet(UUID, 0)).await;
        assert_eq!(client.recv().await, frame(&encode_connack(0)));
        let started = Instant::now();
        client.expect_closed().await;
        assert!(started.elapsed() >= SHORT / 2, "closed by the idle timeout");
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        assert_eq!(event(&mut events).await, DeviceEvent::Disconnected);
    }

    #[tokio::test]
    async fn stuck_writes_time_out() {
        let (logs, _guard) = capture();
        let limits = Limits {
            write_timeout: SHORT,
            ..Limits::PRODUCTION
        };
        let (s, mut events) = session(limits);
        // 32 bytes of pipe: CONNACK and SUBACK fit, a command does not.
        let (mut client, task) = attach_with_buffer(&s, Some(UUID), 32);
        assert_eq!(client.connect(UUID).await, CONNACK_ACCEPTED);
        assert_eq!(client.subscribe(&device_topic(), 0).await, 0);
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        let result = s.send_packet(&"ab".repeat(500)).await;
        assert!(
            matches!(result, Err(ControlError::Network { .. })),
            "{result:?}"
        );
        assert_eq!(event(&mut events).await, DeviceEvent::Disconnected);
        tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
        assert!(
            logs.text().contains("mqtt: write timed out"),
            "{}",
            logs.text()
        );
    }

    #[tokio::test]
    async fn unspecified_bind_is_refused() {
        for bind in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            let c = MqttConfig {
                bind,
                port: 0,
                pinning: None,
            };
            assert!(c.validate().is_err());
            let started = MqttSession::start(&c, &FIXTURE, UUID).await;
            assert!(
                matches!(started, Err(ControlError::Invalid { .. })),
                "{bind}"
            );
        }
        config().validate().unwrap();
    }

    #[tokio::test]
    async fn connections_are_capped() {
        let broker = start().await;
        let addr = broker.local_addr();
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
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn dropping_the_broker_stops_it() {
        let broker = start().await;
        let addr = broker.local_addr();
        drop(broker);
        for _ in 0..100 {
            if TcpStream::connect(addr).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("listener still open after drop");
    }

    // ---- Acceptance: logging ----

    #[tokio::test]
    async fn logs_show_types_cmds_and_lengths_only() {
        let (logs, _guard) = capture();
        let (s, mut events) = session(Limits::PRODUCTION);
        let mut device = connected(&s).await;
        let secret_hex = "deadbeefcafe1234";
        let unknown = message(UUID, "fooBar", &json!(secret_hex));
        device.publish(&msg_topic(), &unknown).await;
        device
            .publish(&msg_topic(), &message(UUID, "bad cmd!", &json!(secret_hex)))
            .await;
        device
            .publish(&bare_msg_topic(), &message(OTHER, "deploy", &json!({})))
            .await;
        device
            .publish(
                &format!("other/{UUID}/x"),
                &message(UUID, "deploy", &json!({})),
            )
            .await;
        device.publish(&msg_topic(), b"not json").await;
        device.publish(&msg_topic(), fixture!("req_timesync")).await;
        device
            .publish(&bare_msg_topic(), fixture!("device_packet"))
            .await;
        device.ping().await;
        tracing::info!(session = ?s, "debug output");
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        let packet = event(&mut events).await;
        assert!(matches!(packet, DeviceEvent::DevicePacket { .. }));
        tracing::info!(event = ?packet, "debug output");
        // A reserved packet type closes the connection.
        device.send(&[0xf0, 0x02, 0xab, 0xcd]).await;
        device.expect_closed().await;
        assert_eq!(event(&mut events).await, DeviceEvent::Disconnected);

        let text = logs.text();
        // Payloads appear only on trace-level "mqtt: payload" lines, with
        // every did (ours and the stranger's) masked.
        let (payload_lines, other_lines): (Vec<&str>, Vec<&str>) =
            text.lines().partition(|l| l.contains("mqtt: payload"));
        assert!(
            payload_lines.iter().all(|l| l.contains("TRACE")),
            "{payload_lines:?}"
        );
        assert!(
            payload_lines
                .iter()
                .any(|l| l.contains(PACKET_HEX) && l.contains(r#""did":"***""#)),
            "{payload_lines:?}"
        );
        let other_text = other_lines.join("\n");
        for secret in [secret_hex, PACKET_HEX, "abcd"] {
            assert!(
                !other_text.contains(secret),
                "leaked {secret}: {other_text}"
            );
        }
        for expected in [
            format!("cmd=\"fooBar\" len={}", unknown.len()),
            "cmd=\"<invalid>\"".to_owned(),
            "cmd=\"deploy\"".to_owned(),
            "mismatched did".to_owned(),
            "topic=\"other/***/x\"".to_owned(),
            "unparseable device message dropped".to_owned(),
            "cmd=\"req_timesync\"".to_owned(),
            format!("DevicePacket {{ hex_len: {} }}", PACKET_HEX.len()),
            "packet_type=15 len=2".to_owned(),
            "packet=\"SUBSCRIBE\"".to_owned(),
        ] {
            assert!(text.contains(&expected), "missing {expected}: {text}");
        }
        for secret in [UUID, OTHER] {
            assert!(!text.contains(secret), "leaked {secret}: {text}");
        }
    }

    #[test]
    fn payload_redaction_masks_identifiers_at_any_depth() {
        let mut value = json!({
            "did": UUID,
            "cmd": "preDeploy",
            "data": {
                "Mac": "xx:xx",
                "nested": [{"ssid": "home", "keep": 1}],
                "uuid": OTHER,
                "errSSID": "home",
                "bootId": "1234",
                "eepromChecksum": 99,
                "BSSID": "x",
                "macAddr": "x",
                "ipAddr": "x",
                "wifi_ip": "x",
                "deviceSerialNo": "x",
                "apiServer": "x",
                "echo": "192.0.2.5", // privacy:allow (TEST-NET-1)
                "echo6": "2001:db8::1",
                "routes": ["https://controlz.test:443", "ssl://192-0-2-5.sslip.io:8883"],
                "note": "at 192.0.2.5:8883 now", // privacy:allow (TEST-NET-1)
                "hw": "AA-BB-CC-00-11-22", // privacy:allow (fake MAC)
                "name": "ac.example.org",
                "model": "RAC_000000_WW",
                "protocolVer": "4.5",
                "modemVer": "clip_hna_v1.9.223",
                "subErrorCode": "W008",
                "time": "12:30:00",
                "data": "0401aabbccdd",
            },
        });
        redact_ids(&mut value);
        assert_eq!(
            value,
            json!({
                "did": "***",
                "cmd": "preDeploy",
                "data": {
                    "Mac": "***",
                    "nested": [{"ssid": "***", "keep": 1}],
                    "uuid": "***",
                    "errSSID": "***",
                    "bootId": "***",
                    "eepromChecksum": "***",
                    "BSSID": "***",
                    "macAddr": "***",
                    "ipAddr": "***",
                    "wifi_ip": "***",
                    "deviceSerialNo": "***",
                    "apiServer": "***",
                    "echo": "***",
                    "echo6": "***",
                    "routes": ["***", "***"],
                    "note": "***",
                    "hw": "***",
                    "name": "***",
                    "model": "RAC_000000_WW",
                    "protocolVer": "4.5",
                    "modemVer": "clip_hna_v1.9.223",
                    "subErrorCode": "W008",
                    "time": "12:30:00",
                    "data": "0401aabbccdd",
                },
            })
        );
    }

    #[test]
    fn key_segments_split_at_separators_and_case() {
        let segs: Vec<&str> = key_segments("macAddr_ip-v4").collect();
        assert_eq!(segs, ["mac", "Addr", "ip", "v4"]);
        assert!(is_identifying_key("IPAddr") && is_identifying_key("ipAddr"));
        assert!(is_identifying_key("ip") && is_identifying_key("localIp"));
        assert!(!is_identifying_key("tip") && !is_identifying_key("ship_mode"));
    }

    /// T1.5 check (a): `preDeploy` before SUBSCRIBE is answered right after
    /// the SUBACK, once.
    #[tokio::test]
    async fn provisioning_before_subscribe_is_answered_on_subscribe() {
        let (s, mut events) = session(Limits::PRODUCTION);
        let (mut device, _task) = attach(&s, Some(UUID));
        assert_eq!(device.connect(UUID).await, CONNACK_ACCEPTED);
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        device.publish(&prov_topic(), fixture!("predeploy")).await;
        device.ping().await;
        // A subscription that does not cover lime/devices/<uuid> changes nothing.
        assert_eq!(device.subscribe("other/topic", 1).await, 1);
        device.ping().await;
        assert_eq!(device.subscribe(&device_topic(), 1).await, 1);
        let reply = device.recv_publish().await;
        assert_eq!(reply.topic, device_topic());
        let body: Value = serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(body["cmd"], "completeProvisioning");
        assert_eq!(body["data"]["provisioningType"], "preDeploy");
        device.send(&encode_puback(reply.packet_id.unwrap())).await;
        // Subscribing again sends nothing more.
        assert_eq!(device.subscribe(&device_topic(), 1).await, 1);
        device.ping().await;
        device
            .publish(&prov_topic(), fixture!("complete_provisioning_ack"))
            .await;
        assert_eq!(
            event(&mut events).await,
            DeviceEvent::Provisioned {
                kind: ProvisioningKind::PreDeploy
            }
        );
    }

    /// T1.5 check (b): no `did` is accepted on our per-uuid topics; another
    /// `did` is still dropped.
    #[tokio::test]
    async fn messages_without_did_are_accepted() {
        let (s, mut events) = session(Limits::PRODUCTION);
        let mut device = connected(&s).await;
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        let without_did = |cmd: &str, data: Value| {
            json!({"mid": 1, "cmd": cmd, "type": 0, "data": data})
                .to_string()
                .into_bytes()
        };
        device
            .publish(&prov_topic(), &without_did("preDeploy", json!({})))
            .await;
        let reply = device.recv_publish().await;
        device.send(&encode_puback(reply.packet_id.unwrap())).await;
        device
            .publish(
                &prov_topic(),
                &without_did("completeProvisioning_ack", json!({"result": 0})),
            )
            .await;
        assert_eq!(
            event(&mut events).await,
            DeviceEvent::Provisioned {
                kind: ProvisioningKind::PreDeploy
            }
        );
        device
            .publish(&msg_topic(), &without_did("device_packet", json!("00ff")))
            .await;
        assert_eq!(
            event(&mut events).await,
            DeviceEvent::DevicePacket {
                hex: "00ff".to_owned()
            }
        );
        device
            .publish(
                &msg_topic(),
                &message(OTHER, "device_packet", &json!("00ff")),
            )
            .await;
        device.ping().await;
        no_event(&mut events);
    }

    // ---- Protocol behaviour (in memory) ----

    #[tokio::test]
    async fn protocol_errors_close_the_connection() {
        let (s, mut events) = session(Limits::PRODUCTION);
        let mut qos2 = Vec::new();
        put_str(&mut qos2, &msg_topic());
        qos2.extend_from_slice(&[0, 1]);
        let cases: [(&str, Vec<u8>); 8] = [
            ("QoS 2 PUBLISH", encode(PUBLISH << 4 | 0b0100, &qos2)),
            ("PUBREL", encode(PUBREL << 4 | 0b0010, &[0, 1])),
            ("PUBREC", encode(PUBREC << 4, &[0, 1])),
            ("second CONNECT", connect_packet(UUID, 60)),
            ("reserved type 0", vec![0x00, 0x00]),
            ("over-long length", vec![0x30, 0xff, 0xff, 0xff, 0xff, 0x01]),
            ("too large", vec![0x30, 0x81, 0x80, 0x02]),
            (
                "nine filters",
                subscribe_packet(1, &[("a", 0); MAX_FILTERS_PER_PACKET + 1]),
            ),
        ];
        for (what, bytes) in cases {
            let mut device = connected(&s).await;
            assert_eq!(event(&mut events).await, DeviceEvent::Connected, "{what}");
            device.send(&bytes).await;
            device.expect_closed().await;
            assert_eq!(
                event(&mut events).await,
                DeviceEvent::Disconnected,
                "{what}"
            );
        }
    }

    #[tokio::test]
    async fn mqtt_3_1_client_connects_and_subscribes() {
        let (logs, _guard) = capture();
        let (s, mut events) = session(Limits::PRODUCTION);
        let (mut device, _task) = attach(&s, Some(UUID));
        // `connect_as` also checks session present = 0 (reserved in 3.1).
        assert_eq!(device.connect_as("MQIsdp", 3, UUID).await, CONNACK_ACCEPTED);
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        assert_eq!(device.subscribe(&device_topic(), 1).await, 1);
        device.ping().await;
        let line = logs
            .text()
            .lines()
            .find(|l| l.contains("mqtt: client speaks"))
            .map_or_default(str::to_owned);
        assert!(line.contains(" DEBUG "), "{line}");
        assert!(line.ends_with("mqtt: client speaks MQTT 3.1"), "{line}");
    }

    #[tokio::test]
    async fn connect_is_checked_first() {
        let (logs, _guard) = capture();
        let (s, mut events) = session(Limits::PRODUCTION);
        // Mixed-up 3.1/3.1.1 names and levels, and MQTT 5, get CONNACK 0x01;
        // the warning names what the client sent.
        for (name, level) in [("MQTT", 3), ("MQIsdp", 4), ("MQTT", 5)] {
            let (mut client, _task) = attach(&s, Some(UUID));
            client
                .send(&encode(
                    CONNECT << 4,
                    &connect_body(name, level, 0x02, UUID),
                ))
                .await;
            assert_eq!(
                client.recv().await,
                frame(&encode_connack(CONNACK_BAD_PROTOCOL))
            );
            client.expect_closed().await;
            let needle = format!(
                "mqtt: CONNECT refused: not MQTT 3.1.1 or 3.1 protocol={name} level={level}"
            );
            let text = logs.text();
            let line = text.lines().find(|l| l.contains(&needle));
            assert!(
                line.is_some_and(|l| l.contains(" WARN ")),
                "no {needle:?} warning in: {text}"
            );
        }
        // Any other first packet, or a malformed CONNECT: closed, no CONNACK.
        for bytes in [
            vec![PINGREQ << 4, 0],
            encode(CONNECT << 4, &connect_body("MQTT", 4, 0x01, UUID)),
        ] {
            let (mut client, _task) = attach(&s, Some(UUID));
            client.send(&bytes).await;
            let mut buf = [0_u8; 8];
            let read = tokio::time::timeout(WAIT, client.stream.read(&mut buf))
                .await
                .unwrap();
            assert!(matches!(read, Ok(0) | Err(_)), "closed without a reply");
        }
        no_event(&mut events);
    }

    #[tokio::test]
    async fn subscriptions_decide_qos_and_delivery() {
        let (s, _events) = session(Limits::PRODUCTION);
        let (mut device, _task) = attach(&s, Some(UUID));
        assert_eq!(device.connect(UUID).await, CONNACK_ACCEPTED);

        // Not subscribed: no completeProvisioning yet, and commands fail.
        device.publish(&prov_topic(), fixture!("predeploy")).await;
        device.ping().await;
        assert!(matches!(
            s.send_packet("00").await,
            Err(ControlError::Network { .. })
        ));

        // QoS 0 wildcard subscription: the held-back reply follows the
        // SUBACK at QoS 0. A QoS 1 request gets its PUBACK first.
        assert_eq!(device.subscribe("lime/devices/+", 0).await, 0);
        let held_back = device.recv_publish().await;
        assert_eq!((held_back.qos, held_back.packet_id), (0, None));
        device
            .send(&publish_qos1(&prov_topic(), fixture!("predeploy"), 42))
            .await;
        assert_eq!(device.recv().await, frame(&encode_puback(42)));
        let reply = device.recv_publish().await;
        assert_eq!((reply.qos, reply.packet_id), (0, None));

        // Unsubscribed again: nothing is sent.
        device.send(&unsubscribe_packet(9, "lime/devices/+")).await;
        assert_eq!(device.recv().await, frame(&encode_unsuback(9)));
        device.publish(&prov_topic(), fixture!("deploy")).await;
        device.ping().await;

        // Requested QoS 2 is granted as 1 (and the deploy reply held back
        // while unsubscribed follows at QoS 1); past the cap, 0x80.
        assert_eq!(device.subscribe("lime/#", 2).await, 1);
        let held_back = device.recv_publish().await;
        assert_eq!(held_back.qos, 1);
        let body: Value = serde_json::from_slice(&held_back.payload).unwrap();
        assert_eq!(body["data"]["provisioningType"], "deploy");
        // 1 (lime/#) + 8 + 7 fill the 16 slots; the 16th new filter fails.
        let filters: Vec<String> = (0..MAX_SUBSCRIPTIONS).map(|i| format!("f/{i}")).collect();
        let (first, second) = filters.split_at(MAX_FILTERS_PER_PACKET);
        for (packet_id, chunk, codes) in [
            (3, first, vec![1; 8]),
            (4, second, [vec![1; 7], vec![SUBACK_FAILURE]].concat()),
        ] {
            let pairs: Vec<(&str, u8)> = chunk.iter().map(|f| (f.as_str(), 1)).collect();
            device.send(&subscribe_packet(packet_id, &pairs)).await;
            assert_eq!(
                device.recv().await,
                frame(&encode_suback(packet_id, &codes))
            );
        }
        // An existing filter is still replaceable.
        device.send(&subscribe_packet(5, &[("lime/#", 0)])).await;
        assert_eq!(device.recv().await, frame(&encode_suback(5, &[0])));
    }

    #[tokio::test]
    async fn send_packet_validates_its_input() {
        let (s, _events) = session(Limits::PRODUCTION);
        for bad in ["", "a", "abc", "zz", "0x00", "00 11"] {
            assert!(
                matches!(s.send_packet(bad).await, Err(ControlError::Invalid { .. })),
                "{bad:?}"
            );
        }
        let too_long = "0".repeat(MAX_HEX_CHARS + 2);
        assert!(matches!(
            s.send_packet(&too_long).await,
            Err(ControlError::Invalid { .. })
        ));
        assert!(matches!(
            s.send_packet("00").await,
            Err(ControlError::Network { .. })
        ));
    }

    #[tokio::test]
    async fn device_packet_needs_hex_data_and_ack_needs_a_request() {
        let (s, mut events) = session(Limits::PRODUCTION);
        let mut device = connected(&s).await;
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        for data in [
            json!("xyz"),
            json!("abc"),
            json!({"hex": "00"}),
            json!(null),
        ] {
            device
                .publish(&msg_topic(), &message(UUID, "device_packet", &data))
                .await;
        }
        device
            .publish(&msg_topic(), fixture!("complete_provisioning_ack"))
            .await;
        device.ping().await;
        no_event(&mut events);
    }

    // ---- Trailing NUL (T1.9 device finding) ----

    /// `payload` as the real firmware sends it: plus one NUL terminator.
    fn with_nul(payload: &[u8]) -> Vec<u8> {
        let mut bytes = payload.to_vec();
        bytes.push(0);
        bytes
    }

    #[test]
    fn trailing_nuls_are_trimmed_and_nothing_else() {
        assert_eq!(trim_trailing_nuls(b"{}\0"), b"{}");
        assert_eq!(trim_trailing_nuls(b"{}\0\0\0"), b"{}");
        assert_eq!(trim_trailing_nuls(b"{}"), b"{}");
        assert_eq!(trim_trailing_nuls(b"{\0}\0"), b"{\0}");
        assert_eq!(trim_trailing_nuls(b"\0{}"), b"\0{}");
        assert_eq!(trim_trailing_nuls(b"\0\0"), b"");
        assert_eq!(trim_trailing_nuls(b""), b"");
    }

    /// Every fixture with the real AC's shape parses once its one trailing
    /// NUL is trimmed, and only then.
    #[test]
    fn real_shape_fixtures_parse_with_their_nul() {
        let cases = [
            (fixture!("deploy"), "deploy"),
            (
                fixture!("complete_provisioning_ack"),
                "completeProvisioning_ack",
            ),
            (fixture!("req_timesync"), "req_timesync"),
            (fixture!("device_packet"), "device_packet"),
        ];
        for (bytes, cmd) in cases {
            let sent = with_nul(bytes);
            let m: DeviceMessage = serde_json::from_slice(trim_trailing_nuls(&sent))
                .unwrap_or_else(|e| panic!("{cmd}: {e}"));
            assert_eq!((m.cmd.as_str(), m.did.as_deref()), (cmd, Some(UUID)));
            assert!(
                serde_json::from_slice::<DeviceMessage>(&sent).is_err(),
                "{cmd}: the NUL is what a strict parser rejects"
            );
        }
        let deploy: Value = serde_json::from_slice(fixture!("deploy")).unwrap();
        assert!(deploy["data"]["appInfo"].is_object());
        assert!(deploy["data"]["extraProfile"]["profiling"].is_array());
    }

    /// The real AC's flow over loopback TLS, every device message ending
    /// in a NUL: `deploy` → `completeProvisioning` → ack (on the message
    /// topic, as the real AC sends it) → `device_packet` in uppercase hex.
    #[tokio::test]
    async fn nul_terminated_device_messages_are_accepted_end_to_end() {
        let broker = start().await;
        let id = client_identity(&FIXTURE.client_ca, UUID);
        let mut device = tls_device(&broker, &id).await;

        device
            .publish(&prov_topic(), &with_nul(fixture!("deploy")))
            .await;
        let reply = device.recv_publish().await;
        assert_eq!(reply.topic, device_topic());
        assert_eq!(reply.qos, 1);
        assert_eq!(reply.payload.last(), Some(&b'}'), "we add no NUL");
        let body: Value = serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(body["cmd"], "completeProvisioning");
        assert_eq!(body["did"], UUID);
        assert_eq!(body["data"]["provisioningType"], "deploy");
        device.send(&encode_puback(reply.packet_id.unwrap())).await;

        device
            .publish(
                &msg_topic(),
                &with_nul(fixture!("complete_provisioning_ack")),
            )
            .await;
        assert_eq!(
            next(&broker).await,
            DeviceEvent::Provisioned {
                kind: ProvisioningKind::Deploy
            }
        );
        device
            .publish(&msg_topic(), &with_nul(fixture!("req_timesync")))
            .await;
        device
            .publish(&msg_topic(), &with_nul(fixture!("device_packet")))
            .await;
        assert!(PACKET_HEX.bytes().any(|b| b.is_ascii_uppercase()));
        assert_eq!(
            next(&broker).await,
            DeviceEvent::DevicePacket {
                hex: PACKET_HEX.to_owned()
            }
        );
        broker.shutdown().await;
    }

    #[tokio::test]
    async fn inner_nuls_and_nul_only_payloads_are_still_dropped() {
        let (logs, _guard) = capture();
        let (s, mut events) = session(Limits::PRODUCTION);
        let mut device = connected(&s).await;
        assert_eq!(event(&mut events).await, DeviceEvent::Connected);
        let deploy = fixture!("deploy").to_vec();
        let comma = deploy.iter().position(|&b| b == b',').unwrap();
        let mut inner = deploy.clone();
        inner.insert(comma + 1, 0);
        let inner_and_trailing = with_nul(&inner);
        for payload in [inner, inner_and_trailing, vec![0], vec![0; 4]] {
            device.publish(&prov_topic(), &payload).await;
        }
        // No completeProvisioning came back and no event was emitted.
        device.ping().await;
        no_event(&mut events);
        let text = logs.text();
        assert_eq!(
            text.matches("mqtt: unparseable device message dropped")
                .count(),
            4,
            "{text}"
        );
        assert!(!text.contains("completeProvisioning sent"), "{text}");
    }

    // ---- Codec ----

    async fn read_bytes(bytes: &[u8]) -> std::result::Result<Frame, ReadError> {
        let mut reader = bytes;
        read_frame(&mut reader, WAIT, WAIT).await
    }

    #[tokio::test]
    async fn remaining_length_is_bounded() {
        for len in [0, 1, 127, 128, 16_383, 16_384, MAX_PACKET_BYTES] {
            let body = vec![7_u8; len];
            let bytes = encode(0x30, &body);
            let expected_header = match len {
                0..=127 => 2,
                128..=16_383 => 3,
                _ => 4,
            };
            assert_eq!(bytes.len(), expected_header + len, "{len}");
            let frame = read_bytes(&bytes).await.unwrap();
            assert_eq!((frame.header, frame.body), (0x30, body), "{len}");
        }
        let too_large = encode(0x30, &vec![0; MAX_PACKET_BYTES + 1]);
        assert!(matches!(
            read_bytes(&too_large).await,
            Err(ReadError::TooLarge(n)) if n == MAX_PACKET_BYTES + 1
        ));
        assert!(matches!(
            read_bytes(&[0x30, 0xff, 0xff, 0xff, 0x7f]).await,
            Err(ReadError::TooLarge(268_435_455))
        ));
        assert!(matches!(
            read_bytes(&[0x30, 0x80, 0x80, 0x80, 0x80, 0x01]).await,
            Err(ReadError::BadLength)
        ));
        assert!(matches!(read_bytes(&[]).await, Err(ReadError::Closed)));
        assert!(matches!(
            read_bytes(&[0x30, 0x05, 1]).await,
            Err(ReadError::Io(_))
        ));
    }

    fn decoded(bytes: &[u8]) -> Decoded<Packet> {
        decode(&frame(bytes))
    }

    fn protocol_error(bytes: &[u8]) -> bool {
        matches!(decoded(bytes), Err(DecodeError::Protocol(_)))
    }

    #[test]
    fn connect_decoding() {
        // Will, username and password are parsed and dropped.
        let mut body = Vec::new();
        put_str(&mut body, "MQTT");
        body.extend_from_slice(&[4, 0b1110_1110, 0, 30]);
        put_str(&mut body, UUID);
        put_str(&mut body, "will/topic");
        put_str(&mut body, "will message");
        put_str(&mut body, "user");
        put_str(&mut body, "pass");
        assert_eq!(
            decoded(&encode(CONNECT << 4, &body)),
            Ok(Packet::Connect {
                client_id: UUID.to_owned(),
                keep_alive: 30,
                level: 4
            })
        );
        // MQTT 3.1: same layout; a 36-char client id is fine (no 23 limit).
        assert_eq!(
            decoded(&encode(
                CONNECT << 4,
                &connect_body("MQIsdp", 3, 0x02, UUID)
            )),
            Ok(Packet::Connect {
                client_id: UUID.to_owned(),
                keep_alive: 60,
                level: 3
            })
        );
        assert_eq!(
            decoded(&encode(CONNECT << 4, &connect_body("MQTT", 5, 0x02, UUID))),
            Err(DecodeError::UnacceptableProtocol {
                name: "MQTT".to_owned(),
                level: 5
            })
        );
        // The refused name is logged sanitized and truncated.
        assert_eq!(
            decoded(&encode(
                CONNECT << 4,
                &connect_body("MQ\nTT\u{e9}-0123456789abcdefXYZ", 4, 0x02, UUID)
            )),
            Err(DecodeError::UnacceptableProtocol {
                name: "MQ?TT?-012345678".to_owned(),
                level: 4
            })
        );
        for (flags, what) in [
            (0x03, "reserved flag"),
            (0x42, "password without username"),
            (0x0a, "will QoS without will"),
            (0x22, "will retain without will"),
            (0x1e, "will QoS 3"),
        ] {
            let bytes = encode(CONNECT << 4, &connect_body("MQTT", 4, flags, UUID));
            assert!(protocol_error(&bytes), "{what}");
        }
        let mut trailing = connect_body("MQTT", 4, 0x02, UUID);
        trailing.push(0);
        assert!(protocol_error(&encode(CONNECT << 4, &trailing)));
        assert!(protocol_error(&encode(
            CONNECT << 4 | 1,
            &connect_body("MQTT", 4, 0x02, UUID)
        )));
        assert!(protocol_error(&encode(
            CONNECT << 4,
            &connect_body("MQTT", 4, 0x02, "a\0b")
        )));
    }

    #[test]
    fn unsupported_packet_types_are_errors() {
        for header in [
            0x00_u8, 0xf0, 0x50, 0x62, 0x70, 0x20, 0x90, 0xb0, 0xd0, 0xc1, 0xe1,
        ] {
            assert!(protocol_error(&[header, 0]), "{header:#04x}");
        }
        assert!(protocol_error(&[PINGREQ << 4, 1, 0]), "PINGREQ with a body");
        assert_eq!(decoded(&[PINGREQ << 4, 0]), Ok(Packet::PingReq));
        assert_eq!(decoded(&[DISCONNECT << 4, 0]), Ok(Packet::Disconnect));
        assert_eq!(decoded(&[PUBACK << 4, 2, 0, 5]), Ok(Packet::PubAck));
        assert!(protocol_error(&[PUBACK << 4, 2, 0, 0]), "packet id 0");
    }

    #[test]
    fn publish_decoding() {
        let mut body = Vec::new();
        put_str(&mut body, "a/b");
        body.extend_from_slice(&[0, 9, b'x', b'y']);
        assert_eq!(
            decoded(&encode(PUBLISH << 4 | 0b1011, &body)),
            Ok(Packet::Publish(Publish {
                topic: "a/b".to_owned(),
                qos: 1,
                packet_id: Some(9),
                payload: b"xy".to_vec(),
            }))
        );
        assert_eq!(
            decoded(&encode(PUBLISH << 4, &body)),
            Ok(Packet::Publish(Publish {
                topic: "a/b".to_owned(),
                qos: 0,
                packet_id: None,
                payload: vec![0, 9, b'x', b'y'],
            }))
        );
        for flags in [0b0100, 0b0110, 0b1000] {
            assert!(
                protocol_error(&encode(PUBLISH << 4 | flags, &body)),
                "{flags:#b}"
            );
        }
        for topic in ["a/+", "a/#", ""] {
            let mut body = Vec::new();
            put_str(&mut body, topic);
            assert!(protocol_error(&encode(PUBLISH << 4, &body)), "{topic:?}");
        }
        let mut zero_id = Vec::new();
        put_str(&mut zero_id, "a");
        zero_id.extend_from_slice(&[0, 0]);
        assert!(protocol_error(&encode(PUBLISH << 4 | 0b0010, &zero_id)));
        // Our own encoder round-trips.
        let ours = encode_publish(&device_topic(), b"{}", Some(3));
        assert_eq!(
            decoded(&ours),
            Ok(Packet::Publish(Publish {
                topic: device_topic(),
                qos: 1,
                packet_id: Some(3),
                payload: b"{}".to_vec(),
            }))
        );
    }

    #[test]
    fn subscribe_decoding() {
        assert_eq!(
            decoded(&subscribe_packet(3, &[("a/+", 1), ("#", 2)])),
            Ok(Packet::Subscribe {
                packet_id: 3,
                filters: vec![("a/+".to_owned(), 1), ("#".to_owned(), 2)],
            })
        );
        assert!(protocol_error(&encode(
            SUBSCRIBE << 4,
            &subscribe_body(3, &[("a", 0)])
        )));
        assert!(protocol_error(&subscribe_packet(3, &[])));
        assert!(protocol_error(&subscribe_packet(3, &[("a", 3)])));
        assert!(protocol_error(&subscribe_packet(3, &[("a/#/b", 0)])));
        assert!(protocol_error(&subscribe_packet(0, &[("a", 0)])));
        let long = "a".repeat(MAX_TOPIC_BYTES + 1);
        assert!(protocol_error(&subscribe_packet(3, &[(&long, 0)])));
        assert_eq!(
            decoded(&unsubscribe_packet(4, "a/#")),
            Ok(Packet::Unsubscribe {
                packet_id: 4,
                filters: vec!["a/#".to_owned()],
            })
        );
    }

    /// MQTT 3.1 clients may set DUP on a re-sent SUBSCRIBE/UNSUBSCRIBE;
    /// other flag values stay protocol errors.
    #[test]
    fn subscribe_and_unsubscribe_accept_dup() {
        let sub = subscribe_body(3, &[("a", 1)]);
        assert_eq!(
            decoded(&encode(SUBSCRIBE << 4 | 0b1010, &sub)),
            Ok(Packet::Subscribe {
                packet_id: 3,
                filters: vec![("a".to_owned(), 1)],
            })
        );
        let mut unsub = 4_u16.to_be_bytes().to_vec();
        put_str(&mut unsub, "a/#");
        assert_eq!(
            decoded(&encode(UNSUBSCRIBE << 4 | 0b1010, &unsub)),
            Ok(Packet::Unsubscribe {
                packet_id: 4,
                filters: vec!["a/#".to_owned()],
            })
        );
        for flags in [0b0000, 0b0011, 0b1000, 0b1110] {
            assert!(
                protocol_error(&encode(SUBSCRIBE << 4 | flags, &sub)),
                "{flags:#06b}"
            );
            assert!(
                protocol_error(&encode(UNSUBSCRIBE << 4 | flags, &unsub)),
                "{flags:#06b}"
            );
        }
    }

    #[test]
    fn protocol_labels() {
        assert_eq!(protocol_label(3), "MQTT 3.1");
        assert_eq!(protocol_label(4), "MQTT 3.1.1");
        assert_eq!(protocol_label(5), "MQTT (unknown level)");
    }

    #[test]
    fn topic_filters_and_matching() {
        for ok in ["a", "a/b", "+", "#", "a/+/c", "a/#", "+/+", "/", "a//b"] {
            assert!(valid_topic_filter(ok), "{ok:?}");
        }
        for bad in ["", "a#", "a/#/b", "a+", "a/b+/c", "##"] {
            assert!(!valid_topic_filter(bad), "{bad:?}");
        }
        let cases = [
            ("lime/devices/x", "lime/devices/x", true),
            ("lime/devices/+", "lime/devices/x", true),
            ("lime/#", "lime/devices/x", true),
            ("lime/devices/#", "lime/devices", true),
            ("#", "lime/devices/x", true),
            ("+/+/+", "lime/devices/x", true),
            ("lime/+", "lime/devices/x", false),
            ("lime/devices/y", "lime/devices/x", false),
            ("lime/devices/x/y", "lime/devices/x", false),
            ("#", "$aws/rules/x", false),
            ("+/rules/x", "$aws/rules/x", false),
            ("$aws/#", "$aws/rules/x", true),
        ];
        for (filter, topic, want) in cases {
            assert_eq!(topic_matches(filter, topic), want, "{filter} vs {topic}");
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(
            redact_topic(&format!("clip/devices/{UUID}/message"), UUID),
            "clip/devices/***/message"
        );
        assert_eq!(loggable_cmd("device_packet"), "device_packet");
        for bad in ["", "a b", "a-b", "x\n", &"a".repeat(33)] {
            assert_eq!(loggable_cmd(bad), "<invalid>", "{bad:?}");
        }
        assert!(is_hex("00aAfF"));
        for bad in ["", "0", "0g", "0x", " 00"] {
            assert!(!is_hex(bad), "{bad:?}");
        }
        let p = Limits::PRODUCTION;
        assert_eq!(p.idle_timeout(0), Duration::from_mins(30));
        assert_eq!(p.idle_timeout(10), Duration::from_secs(30));
        assert_eq!(p.idle_timeout(60), Duration::from_secs(90));
        assert_eq!(p.idle_timeout(1500), Duration::from_mins(30));
        assert_eq!(p.idle_timeout(u16::MAX), Duration::from_mins(30));
    }

    #[test]
    fn uuid_is_validated() {
        let long = "a".repeat(MAX_UUID_BYTES + 1);
        for bad in ["", "a/b", "a b", "a+b", "a#b", "a?b", "a\0b", long.as_str()] {
            assert!(
                MqttSession::new(bad, Limits::PRODUCTION, None).is_err(),
                "{bad:?}"
            );
        }
        let longest = "a".repeat(MAX_UUID_BYTES);
        let (s, _) = MqttSession::new(&longest, Limits::PRODUCTION, None).unwrap();
        for topic in s.topics.provisioning.iter().chain(&s.topics.message) {
            assert!(valid_topic_name(topic), "{topic}");
        }
        assert!(valid_topic_name(&s.topics.device));
    }

    #[test]
    fn fixtures_parse() {
        let cases = [
            (fixture!("predeploy"), "preDeploy"),
            (fixture!("deploy"), "deploy"),
            (
                fixture!("complete_provisioning_ack"),
                "completeProvisioning_ack",
            ),
            (fixture!("req_timesync"), "req_timesync"),
            (fixture!("device_packet"), "device_packet"),
        ];
        for (bytes, cmd) in cases {
            let m: DeviceMessage = serde_json::from_slice(bytes).unwrap();
            assert_eq!((m.cmd.as_str(), m.did.as_deref()), (cmd, Some(UUID)));
        }
        let m: DeviceMessage = serde_json::from_slice(fixture!("device_packet")).unwrap();
        assert_eq!(m.data, json!(PACKET_HEX));
    }

    #[test]
    fn complete_provisioning_snapshot() {
        let topics = Topics::new(UUID);
        let reply = complete_provisioning(
            UUID,
            &topics,
            1_700_000_000_000,
            ProvisioningKind::PreDeploy,
        );
        insta::assert_snapshot!(
            "complete_provisioning",
            serde_json::to_string_pretty(&reply).unwrap()
        );
        let mut deploy =
            complete_provisioning(UUID, &topics, 1_700_000_000_000, ProvisioningKind::Deploy);
        assert_eq!(deploy["data"]["provisioningType"], "deploy");
        deploy["data"]["provisioningType"] = json!("preDeploy");
        assert_eq!(deploy, reply, "only provisioningType differs");
        assert_eq!(
            packet_command(UUID, 5, "00ff"),
            json!({"did": UUID, "mid": 5, "cmd": "packet", "type": 1, "data": "00ff"})
        );
    }
}
