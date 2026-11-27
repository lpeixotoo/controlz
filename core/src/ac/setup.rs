//! `SoftAP` setup client for the LG Wi-Fi module (KB: AC — local control
//! options, setup protocol).
//!
//! While the AC is in setup mode, the module serves TLS on port 5500 of its
//! `SoftAP` gateway. Messages are JSON objects written back to back with no
//! delimiter: `{"type":"request"|"response","cmd":…,"data":{…}}`. The module
//! also sends unsolicited `ping` requests, which we answer with `pong`.
//!
//! Only [`read_device_info`] is public: it sends nothing that changes the
//! module. The provisioning steps (`setCertInfo`, `setApInfo`, `releaseDev`)
//! make the module store a `registered` flag that only a Wi-Fi reset clears,
//! so they stay crate-private until the local cloud exists (T1.9).

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use openssl::base64;
use openssl::rsa::Rsa;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use super::tls;
use crate::error::{ControlError, Result};
use crate::secret::Secret;

/// The module's setup port on its `SoftAP` gateway.
pub const SETUP_PORT: u16 = 5500;

/// Upper bound for the TCP connect to the module.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound for the module's reply to one request.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound for sending TLS `close_notify`; a peer that stopped reading
/// could otherwise block it on a full send buffer.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Longest `result`/`code` value quoted in an error message.
const MAX_STATUS_CHARS: usize = 32;

/// Largest JSON object we buffer before giving up on the peer.
const MAX_FRAME: usize = 64 * 1024;

/// Bytes read from the socket per call.
const READ_CHUNK: usize = 4096;

/// `constantConnect:"Y"` keeps the connection open between commands.
const CONSTANT_CONNECT: &str = "Y";

/// Splits a byte stream of concatenated JSON objects into single objects.
#[derive(Debug, Default)]
struct FrameSplitter {
    buf: Vec<u8>,
}

impl FrameSplitter {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete object, or `None` if more bytes are needed.
    ///
    /// # Errors
    /// [`ControlError::Device`] for anything that is not a JSON object, or
    /// for an object larger than [`MAX_FRAME`].
    fn next_frame(&mut self) -> Result<Option<Value>> {
        let start = self
            .buf
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(self.buf.len());
        self.buf.drain(..start);
        match self.buf.first() {
            None => return Ok(None),
            Some(b'{') => {}
            Some(_) => return Err(device("setup: module sent non-JSON data")),
        }
        let mut stream = serde_json::Deserializer::from_slice(&self.buf).into_iter::<Value>();
        match stream.next() {
            Some(Ok(value)) => {
                let consumed = stream.byte_offset();
                self.buf.drain(..consumed);
                Ok(Some(value))
            }
            Some(Err(e)) if e.is_eof() => {
                if self.buf.len() > MAX_FRAME {
                    Err(device(format!(
                        "setup: module frame exceeds {MAX_FRAME} bytes"
                    )))
                } else {
                    Ok(None)
                }
            }
            Some(Err(e)) => Err(device(format!("setup: invalid JSON from module: {e}"))),
            None => Ok(None),
        }
    }
}

/// A request we send to the module (or `pong`, our answer to its `ping`).
#[cfg_attr(not(test), allow(dead_code))] // provisioning variants are wired up by T1.9
enum Request {
    SetDeviceInit,
    GetDeviceInfo {
        public_key: String,
        api_server: Option<String>,
        mqtt_server: Option<String>,
    },
    SetCertInfo {
        otp: Secret,
        svccode: String,
        svcphase: String,
    },
    SetApInfo {
        ssid: String,
        password: Secret,
    },
    ReleaseDev,
    Pong,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only the command name: payloads carry keys, OTPs and passwords.
        write!(f, "Request({})", self.cmd())
    }
}

#[derive(Serialize)]
struct Envelope<'a, D> {
    #[serde(rename = "type")]
    kind: &'a str,
    cmd: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<D>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetDeviceInitData {
    set: &'static str,
    constant_connect: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetDeviceInfoData<'a> {
    public_key: &'a str,
    constant_connect: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mqtt_server: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_server: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetCertInfoData<'a> {
    otp: &'a str,
    svccode: &'a str,
    svcphase: &'a str,
    constant_connect: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetApInfoData<'a> {
    format: &'static str,
    ssid: &'a str,
    security: &'static str,
    cipher: &'static str,
    password: &'a str,
    constant_connect: &'static str,
}

impl Request {
    fn cmd(&self) -> &'static str {
        match self {
            Self::SetDeviceInit => "setDeviceInit",
            Self::GetDeviceInfo { .. } => "getDeviceInfo",
            Self::SetCertInfo { .. } => "setCertInfo",
            Self::SetApInfo { .. } => "setApInfo",
            Self::ReleaseDev => "releaseDev",
            Self::Pong => "pong",
        }
    }

    /// The wire bytes, with fields in the order the rethink wiki shows.
    fn to_frame(&self) -> Result<Vec<u8>> {
        let cmd = self.cmd();
        match self {
            Self::SetDeviceInit => frame(
                "request",
                cmd,
                Some(SetDeviceInitData {
                    set: "true",
                    constant_connect: CONSTANT_CONNECT,
                }),
            ),
            Self::GetDeviceInfo {
                public_key,
                api_server,
                mqtt_server,
            } => frame(
                "request",
                cmd,
                Some(GetDeviceInfoData {
                    public_key,
                    constant_connect: CONSTANT_CONNECT,
                    mqtt_server: mqtt_server.as_deref(),
                    api_server: api_server.as_deref(),
                }),
            ),
            Self::SetCertInfo {
                otp,
                svccode,
                svcphase,
            } => frame(
                "request",
                cmd,
                Some(SetCertInfoData {
                    otp: otp.expose(),
                    svccode,
                    svcphase,
                    constant_connect: CONSTANT_CONNECT,
                }),
            ),
            Self::SetApInfo { ssid, password } => {
                let ssid = base64::encode_block(ssid.as_bytes());
                let password = base64::encode_block(password.expose().as_bytes());
                frame(
                    "request",
                    cmd,
                    Some(SetApInfoData {
                        format: "B64",
                        ssid: &ssid,
                        security: "WPA2_PSK",
                        cipher: "AES",
                        password: &password,
                        constant_connect: CONSTANT_CONNECT,
                    }),
                )
            }
            Self::ReleaseDev => frame::<()>("request", cmd, None),
            Self::Pong => frame::<()>("response", cmd, None),
        }
    }
}

fn frame<D: Serialize>(kind: &str, cmd: &str, data: Option<D>) -> Result<Vec<u8>> {
    serde_json::to_vec(&Envelope { kind, cmd, data })
        .map_err(|e| ControlError::invalid(format!("setup: encoding {cmd}: {e}")))
}

/// A message from the module.
#[derive(Debug)]
enum Incoming {
    Response {
        cmd: String,
        data: Value,
        /// [`summarize`] of the whole frame, for error messages.
        summary: String,
    },
    Ping,
    /// A frame without `cmd` (possibly an error reply); skipped, but its
    /// [`summarize`] is kept so a later timeout can report it.
    Unnamed {
        summary: String,
    },
    /// Any other request; ignored.
    Other {
        kind: String,
        cmd: String,
    },
}

impl Incoming {
    fn parse(value: Value) -> Result<Self> {
        let Value::Object(mut obj) = value else {
            return Err(device("setup: module sent a non-object frame"));
        };
        let summary = summarize(&obj);
        let text = |v: Option<Value>| match v {
            Some(Value::String(s)) => s,
            _ => String::new(),
        };
        let kind = text(obj.remove("type"));
        let cmd = text(obj.remove("cmd"));
        if cmd.is_empty() {
            return Ok(Self::Unnamed { summary });
        }
        Ok(match kind.as_str() {
            "response" => Self::Response {
                cmd,
                data: obj.remove("data").unwrap_or(Value::Null),
                summary,
            },
            "request" if cmd == "ping" => Self::Ping,
            _ => Self::Other { kind, cmd },
        })
    }
}

/// Describes a frame for errors and logs without leaking identifiers: the
/// key names, plus short `result`/`code` values (the only values that are
/// status, not data).
fn summarize(obj: &Map<String, Value>) -> String {
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    let mut parts = vec![format!("keys [{}]", keys.join(", "))];
    for field in ["result", "code"] {
        let value: String = match obj.get(field) {
            Some(Value::String(s)) => s.chars().take(MAX_STATUS_CHARS).collect(),
            Some(v @ (Value::Number(_) | Value::Bool(_))) => v.to_string(),
            _ => continue,
        };
        parts.push(format!("{field}={value}"));
    }
    parts.join(", ")
}

/// What the module reports in its `getDeviceInfo` response.
///
/// `Debug` hides `uuid`, `encrypt_val` and the values in `other` (which
/// include the MAC address), so this can be logged safely.
#[derive(Clone)]
pub struct DeviceInfo {
    /// `ThinQ` model, e.g. `RAC_056905_WW`.
    pub model_name: String,
    /// The module's `ThinQ` device id. Private: keep it out of logs and the repo.
    pub uuid: Option<String>,
    pub software_ver: Option<String>,
    /// Base64 token encrypted with the public key we sent.
    pub encrypt_val: Option<String>,
    /// `|`-separated diagnostics; ends in `encryptRes:0` when the key was accepted.
    pub extra: Option<String>,
    /// Every other field, as text (non-string values are JSON-encoded).
    pub other: BTreeMap<String, String>,
}

impl fmt::Debug for DeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mask = |v: &Option<String>| v.as_ref().map(|_| "***");
        f.debug_struct("DeviceInfo")
            .field("model_name", &self.model_name)
            .field("uuid", &mask(&self.uuid))
            .field("software_ver", &self.software_ver)
            .field("encrypt_val", &mask(&self.encrypt_val))
            .field("extra", &self.extra)
            .field("other", &self.other.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl DeviceInfo {
    /// `summary` describes the enclosing frame, for the error message.
    fn from_data(data: Value, summary: &str) -> Result<Self> {
        let Value::Object(obj) = data else {
            return Err(device(format!(
                "setup: getDeviceInfo response has no data ({summary})"
            )));
        };
        let data_summary = summarize(&obj);
        let mut fields: BTreeMap<String, String> = obj
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                (k, v)
            })
            .collect();
        let model_name = fields.remove("modelName").ok_or_else(|| {
            device(format!(
                "setup: getDeviceInfo response has no modelName (data {data_summary})"
            ))
        })?;
        Ok(Self {
            model_name,
            uuid: fields.remove("uuid"),
            software_ver: fields.remove("softwareVer"),
            encrypt_val: fields.remove("encrypt_val"),
            extra: fields.remove("extra"),
            other: fields,
        })
    }
}

/// A setup-protocol session over any byte stream (TLS in production).
struct SetupClient<S> {
    stream: S,
    splitter: FrameSplitter,
    response_timeout: Duration,
    shutdown_timeout: Duration,
}

impl<S> fmt::Debug for SetupClient<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetupClient")
            .field("buffered", &self.splitter.buf.len())
            .finish_non_exhaustive()
    }
}

#[cfg_attr(not(test), allow(dead_code))] // provisioning steps are wired up by T1.9
impl<S> SetupClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn new(stream: S) -> Self {
        Self {
            stream,
            splitter: FrameSplitter::default(),
            response_timeout: RESPONSE_TIMEOUT,
            shutdown_timeout: SHUTDOWN_TIMEOUT,
        }
    }

    async fn send(&mut self, request: &Request) -> Result<()> {
        let bytes = request.to_frame()?;
        tracing::debug!(cmd = request.cmd(), "setup: send");
        self.stream
            .write_all(&bytes)
            .await
            .map_err(|e| ControlError::network(format!("setup: write failed: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| ControlError::network(format!("setup: write failed: {e}")))
    }

    async fn next_frame(&mut self) -> Result<Value> {
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            if let Some(value) = self.splitter.next_frame()? {
                return Ok(value);
            }
            let n = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|e| ControlError::network(format!("setup: read failed: {e}")))?;
            if n == 0 {
                return Err(ControlError::network("setup: module closed the connection"));
            }
            self.splitter.push(&chunk[..n]);
        }
    }

    /// Sends `request` and returns the `data` of the matching response.
    /// Pings that arrive meanwhile are answered; other frames are skipped.
    async fn request(&mut self, request: &Request) -> Result<(Value, String)> {
        self.send(request).await?;
        let expected = request.cmd();
        let limit = self.response_timeout;
        let mut last_unnamed: Option<String> = None;
        let wait = async {
            loop {
                match Incoming::parse(self.next_frame().await?)? {
                    Incoming::Response { cmd, data, summary } if cmd == expected => {
                        return Ok((data, summary));
                    }
                    Incoming::Ping => {
                        tracing::debug!("setup: ping");
                        self.send(&Request::Pong).await?;
                    }
                    Incoming::Response { cmd, .. } => {
                        tracing::debug!(cmd, "setup: skipping unexpected response");
                    }
                    Incoming::Other { kind, cmd } => {
                        tracing::debug!(kind, cmd, "setup: skipping frame");
                    }
                    Incoming::Unnamed { summary } => {
                        tracing::debug!(summary, "setup: skipping frame without cmd");
                        last_unnamed = Some(summary);
                    }
                }
            }
        };
        let outcome = tokio::time::timeout(limit, wait).await;
        outcome.unwrap_or_else(|_| {
            let message = match last_unnamed {
                Some(summary) => format!(
                    "setup: no {expected} response in time; last frame without cmd had {summary}"
                ),
                None => format!("setup: no {expected} response in time"),
            };
            Err(ControlError::network(message))
        })
    }

    async fn set_device_init(&mut self) -> Result<()> {
        self.request(&Request::SetDeviceInit).await.map(drop)
    }

    async fn get_device_info(
        &mut self,
        public_key: String,
        api_server: Option<String>,
        mqtt_server: Option<String>,
    ) -> Result<DeviceInfo> {
        let (data, summary) = self
            .request(&Request::GetDeviceInfo {
                public_key,
                api_server,
                mqtt_server,
            })
            .await?;
        DeviceInfo::from_data(data, &summary)
    }

    async fn set_cert_info(&mut self, otp: Secret, svccode: String) -> Result<()> {
        self.request(&Request::SetCertInfo {
            otp,
            svccode,
            svcphase: "OP".to_owned(),
        })
        .await
        .map(drop)
    }

    async fn set_ap_info(&mut self, ssid: String, password: Secret) -> Result<()> {
        self.request(&Request::SetApInfo { ssid, password })
            .await
            .map(drop)
    }

    async fn release_dev(&mut self) -> Result<()> {
        self.request(&Request::ReleaseDev).await.map(drop)
    }

    async fn shutdown(&mut self) {
        // Best effort: we already have what we came for, so a failed or
        // stuck close_notify is ignored.
        let _ = tokio::time::timeout(self.shutdown_timeout, self.stream.shutdown()).await;
    }
}

/// Connects to the module's setup port and reads its device info.
///
/// Sends only `getDeviceInfo` (without `apiServer`/`mqttServer`), preceded by
/// `setDeviceInit` when `init` is set. Neither provisions the module. The
/// public key sent is a throwaway RSA-2048 key that is never stored.
///
/// # Errors
/// [`ControlError::Network`] if the module cannot be reached, the TLS
/// handshake fails or a reply times out; [`ControlError::Device`] if the
/// module's reply is malformed.
pub async fn read_device_info(host: &str, port: u16, init: bool) -> Result<DeviceInfo> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| ControlError::network(format!("setup: connect to {host}:{port} timed out")))?
        .map_err(|e| ControlError::network(format!("setup: connect to {host}:{port}: {e}")))?;
    let connector = tls::module_connector()?;
    let tls = tls::connect(&connector, host, tcp).await?;
    let mut client = SetupClient::new(tls);
    let public_key = throwaway_public_key()?;
    let result = async {
        if init {
            client.set_device_init().await?;
        }
        client.get_device_info(public_key, None, None).await
    }
    .await;
    client.shutdown().await;
    result
}

/// A PEM public key (`SubjectPublicKeyInfo`) for a fresh RSA-2048 key that is
/// dropped right away. PEM lines never start with whitespace, which the
/// module rejects (`extra` ends in `ffff`).
fn throwaway_public_key() -> Result<String> {
    let invalid = |e: openssl::error::ErrorStack| ControlError::invalid(format!("setup key: {e}"));
    let rsa = Rsa::generate(2048).map_err(invalid)?;
    let pem = rsa.public_key_to_pem().map_err(invalid)?;
    String::from_utf8(pem).map_err(ControlError::invalid)
}

fn device(message: impl fmt::Display) -> ControlError {
    ControlError::Device {
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ac::pki::test_fixture::FIXTURE;
    use tokio::io::DuplexStream;

    const FAKE_KEY: &str =
        "-----BEGIN PUBLIC KEY-----\nRkFLRS1QVUJMSUMtS0VZ\n-----END PUBLIC KEY-----\n";
    const FAKE_OTP: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    const FAKE_SSID: &str = "fake-ssid";
    const FAKE_PASSWORD: &str = "fake-password";
    const FAKE_UUID: &str = "00000000-0000-0000-0000-000000000000";

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/ac_setup/",
                $name,
                ".json"
            ))
        };
    }

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn encoded(request: &Request) -> Value {
        serde_json::from_slice(&request.to_frame().unwrap()).unwrap()
    }

    fn get_device_info(servers: bool) -> Request {
        Request::GetDeviceInfo {
            public_key: FAKE_KEY.to_owned(),
            api_server: servers.then(|| "https://controlz.test:443".to_owned()),
            mqtt_server: servers.then(|| "ssl://controlz.test:8883".to_owned()),
        }
    }

    fn set_cert_info() -> Request {
        Request::SetCertInfo {
            otp: Secret::new(FAKE_OTP),
            svccode: "SVC202".to_owned(),
            svcphase: "OP".to_owned(),
        }
    }

    fn set_ap_info() -> Request {
        Request::SetApInfo {
            ssid: FAKE_SSID.to_owned(),
            password: Secret::new(FAKE_PASSWORD),
        }
    }

    #[test]
    fn requests_match_fixtures() {
        let cases = [
            (Request::SetDeviceInit, fixture!("set_device_init")),
            (get_device_info(false), fixture!("get_device_info")),
            (get_device_info(true), fixture!("get_device_info_servers")),
            (set_cert_info(), fixture!("set_cert_info")),
            (set_ap_info(), fixture!("set_ap_info")),
            (Request::ReleaseDev, fixture!("release_dev")),
            (Request::Pong, fixture!("pong")),
        ];
        for (request, expected) in cases {
            assert_eq!(encoded(&request), json(expected), "{}", request.cmd());
        }
    }

    #[test]
    fn requests_keep_wiki_field_order() {
        let text = String::from_utf8(set_ap_info().to_frame().unwrap()).unwrap();
        assert!(
            text.starts_with(r#"{"type":"request","cmd":"setApInfo","data":{"format":"B64","#),
            "{text}"
        );
    }

    #[test]
    fn request_debug_hides_payload() {
        for request in [set_cert_info(), set_ap_info()] {
            let out = format!("{request:?}");
            assert!(
                !out.contains(FAKE_OTP) && !out.contains(FAKE_PASSWORD),
                "{out}"
            );
        }
    }

    #[test]
    fn incoming_frames_parse() {
        assert!(matches!(
            Incoming::parse(json(fixture!("ping"))).unwrap(),
            Incoming::Ping
        ));
        let Incoming::Response { cmd, data, summary } =
            Incoming::parse(json(fixture!("get_device_info_response"))).unwrap()
        else {
            panic!("not a response");
        };
        assert_eq!(cmd, "getDeviceInfo");
        assert_eq!(summary, "keys [cmd, data, type]");
        let info = DeviceInfo::from_data(data, &summary).unwrap();
        assert_eq!(info.model_name, "RAC_000000_WW");
        assert_eq!(info.uuid.as_deref(), Some(FAKE_UUID));
        assert_eq!(info.software_ver.as_deref(), Some("000000"));
        assert_eq!(
            info.encrypt_val.as_deref(),
            Some("ZmFrZS1lbmNyeXB0LXZhbA==")
        );
        assert_eq!(
            info.extra.as_deref(),
            Some("FAKE_RESET_REASON|0|encryptRes:0")
        );
        assert_eq!(info.other["protocolVer"], "4.5");
        assert!(!info.other.contains_key("modelName"));
        assert!(matches!(
            Incoming::parse(json(r#"{"type":"request","cmd":"other"}"#)).unwrap(),
            Incoming::Other { .. }
        ));
    }

    #[test]
    fn frame_without_cmd_is_kept_as_summary() {
        let frame = json(&format!(
            r#"{{"type":"response","result":"FAIL","code":-3,"uuid":"{FAKE_UUID}","data":{{"x":1}}}}"#
        ));
        let Incoming::Unnamed { summary } = Incoming::parse(frame).unwrap() else {
            panic!("not unnamed");
        };
        assert_eq!(
            summary,
            "keys [code, data, result, type, uuid], result=FAIL, code=-3"
        );
    }

    #[test]
    fn summary_truncates_status_and_skips_objects() {
        let long = "E".repeat(100);
        let frame = json(&format!(r#"{{"result":"{long}","code":{{"a":1}}}}"#));
        let Value::Object(obj) = frame else {
            unreachable!()
        };
        assert_eq!(
            summarize(&obj),
            format!(
                "keys [code, result], result={}",
                "E".repeat(MAX_STATUS_CHARS)
            )
        );
    }

    #[test]
    fn device_info_errors_keep_status_not_ids() {
        let err = DeviceInfo::from_data(
            json(&format!(
                r#"{{"uuid":"{FAKE_UUID}","result":"NG","code":7}}"#
            )),
            "keys [cmd, data, type]",
        )
        .unwrap_err();
        assert!(matches!(err, ControlError::Device { .. }), "{err:?}");
        let text = err.to_string();
        assert!(
            text.contains("no modelName (data keys [code, result, uuid], result=NG, code=7)"),
            "{text}"
        );
        assert!(!text.contains(FAKE_UUID), "{text}");

        let err =
            DeviceInfo::from_data(Value::Null, "keys [cmd, code, type], code=12").unwrap_err();
        assert!(
            err.to_string()
                .contains("has no data (keys [cmd, code, type], code=12)"),
            "{err}"
        );
    }

    #[test]
    fn device_info_debug_hides_ids() {
        let data = json(fixture!("get_device_info_response"))["data"].clone();
        let out = format!("{:?}", DeviceInfo::from_data(data, "").unwrap());
        assert!(out.contains("RAC_000000_WW"), "{out}");
        for secret in [FAKE_UUID, "ZmFrZS1lbmNyeXB0LXZhbA==", "xx:xx:xx"] {
            assert!(!out.contains(secret), "leaked {secret}: {out}");
        }
    }

    fn drain(splitter: &mut FrameSplitter) -> Vec<Value> {
        std::iter::from_fn(|| splitter.next_frame().unwrap()).collect()
    }

    #[test]
    fn splitter_byte_by_byte() {
        let text = fixture!("get_device_info_response");
        let mut splitter = FrameSplitter::default();
        let mut frames = Vec::new();
        for byte in text.as_bytes() {
            splitter.push(std::slice::from_ref(byte));
            frames.extend(drain(&mut splitter));
        }
        assert_eq!(frames, vec![json(text)]);
    }

    #[test]
    fn splitter_merged_and_whitespace() {
        let mut splitter = FrameSplitter::default();
        let merged = format!(
            "{}{}\n \r\n{}",
            fixture!("ping").trim(),
            fixture!("get_device_info_response").trim(),
            fixture!("ping").trim()
        );
        splitter.push(merged.as_bytes());
        let frames = drain(&mut splitter);
        assert_eq!(
            frames,
            vec![
                json(fixture!("ping")),
                json(fixture!("get_device_info_response")),
                json(fixture!("ping")),
            ]
        );
        assert_eq!(splitter.buf, b"");
    }

    #[test]
    fn splitter_caps_frame_size() {
        let mut splitter = FrameSplitter::default();
        splitter.push(br#"{"cmd":""#);
        splitter.push(&vec![b'a'; MAX_FRAME]);
        assert!(matches!(
            splitter.next_frame(),
            Err(ControlError::Device { .. })
        ));
    }

    #[test]
    fn splitter_rejects_non_json() {
        for bad in [&b"<xml/>"[..], b"[1,2]", b"{\"a\":}"] {
            let mut splitter = FrameSplitter::default();
            splitter.push(bad);
            assert!(splitter.next_frame().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn throwaway_key_is_unindented_pem() {
        let pem = throwaway_public_key().unwrap();
        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----\n"), "{pem}");
        assert!(pem.lines().all(|l| !l.starts_with(char::is_whitespace)));
    }

    /// The module side of a session: reads frames and writes raw replies.
    struct MockModule {
        stream: DuplexStream,
        splitter: FrameSplitter,
    }

    impl MockModule {
        async fn expect(&mut self, cmd: &str) -> Value {
            let mut chunk = [0u8; 512];
            loop {
                if let Some(v) = self.splitter.next_frame().unwrap() {
                    assert_eq!(v["cmd"], cmd, "unexpected frame {v}");
                    return v;
                }
                let n = self.stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed while waiting for {cmd}");
                self.splitter.push(&chunk[..n]);
            }
        }

        async fn write(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).await.unwrap();
            self.stream.flush().await.unwrap();
        }

        async fn respond(&mut self, cmd: &str) {
            let frame = format!(r#"{{"type":"response","cmd":"{cmd}","data":{{"result":"OK"}}}}"#);
            self.write(frame.as_bytes()).await;
        }
    }

    #[tokio::test]
    async fn mock_module_full_sequence() {
        let (client_end, module_end) = tokio::io::duplex(1024);
        let module = tokio::spawn(async move {
            let mut m = MockModule {
                stream: module_end,
                splitter: FrameSplitter::default(),
            };
            let init = m.expect("setDeviceInit").await;
            assert_eq!(init, json(fixture!("set_device_init")));
            m.respond("setDeviceInit").await;

            // Unsolicited ping, then the response split in two writes.
            let info = m.expect("getDeviceInfo").await;
            assert_eq!(info, json(fixture!("get_device_info_servers")));
            m.write(fixture!("ping").trim().as_bytes()).await;
            m.expect("pong").await;
            let reply = fixture!("get_device_info_response").as_bytes();
            let (a, b) = reply.split_at(reply.len() / 2);
            m.write(a).await;
            tokio::task::yield_now().await;
            m.write(b).await;

            // Ping, a stray response and the real one merged in one write.
            m.expect("setCertInfo").await;
            let merged = format!(
                r#"{}{{"type":"response","cmd":"other"}}{{"type":"response","cmd":"setCertInfo","data":{{}}}}"#,
                fixture!("ping").trim()
            );
            m.write(merged.as_bytes()).await;
            m.expect("pong").await;

            let ap = m.expect("setApInfo").await;
            assert_eq!(ap, json(fixture!("set_ap_info")));
            let password = ap["data"]["password"].as_str().unwrap();
            assert_eq!(
                base64::decode_block(password).unwrap(),
                FAKE_PASSWORD.as_bytes()
            );
            m.respond("setApInfo").await;

            m.expect("releaseDev").await;
            m.respond("releaseDev").await;
        });

        let mut client = SetupClient::new(client_end);
        client.set_device_init().await.unwrap();
        let info = client
            .get_device_info(
                FAKE_KEY.to_owned(),
                Some("https://controlz.test:443".to_owned()),
                Some("ssl://controlz.test:8883".to_owned()),
            )
            .await
            .unwrap();
        assert_eq!(info.model_name, "RAC_000000_WW");
        client
            .set_cert_info(Secret::new(FAKE_OTP), "SVC202".to_owned())
            .await
            .unwrap();
        client
            .set_ap_info(FAKE_SSID.to_owned(), Secret::new(FAKE_PASSWORD))
            .await
            .unwrap();
        client.release_dev().await.unwrap();
        module.await.unwrap();
    }

    #[tokio::test]
    async fn closed_connection_is_a_network_error() {
        let (client_end, module_end) = tokio::io::duplex(1024);
        drop(module_end);
        let mut client = SetupClient::new(client_end);
        assert!(matches!(
            client.set_device_init().await,
            Err(ControlError::Network { .. })
        ));
    }

    #[tokio::test]
    async fn silent_module_times_out() {
        let (client_end, _module_end) = tokio::io::duplex(1024);
        let mut client = SetupClient::new(client_end);
        client.response_timeout = Duration::from_millis(50);
        let err = client.set_device_init().await.unwrap_err();
        assert!(err.to_string().contains("in time"), "{err}");
    }

    #[tokio::test]
    async fn frame_without_cmd_is_skipped_and_reported_on_timeout() {
        let (client_end, module_end) = tokio::io::duplex(1024);
        let module = tokio::spawn(async move {
            let mut m = MockModule {
                stream: module_end,
                splitter: FrameSplitter::default(),
            };
            m.expect("setDeviceInit").await;
            m.write(br#"{"type":"response","result":"FAIL"}"#).await;
            m.respond("setDeviceInit").await;
            m.expect("releaseDev").await;
            m.write(br#"{"type":"response","code":500}"#).await;
            m
        });
        let mut client = SetupClient::new(client_end);
        client.response_timeout = Duration::from_millis(200);
        client.set_device_init().await.unwrap();
        let err = client.release_dev().await.unwrap_err();
        assert!(matches!(err, ControlError::Network { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains("no releaseDev response in time; last frame without cmd had keys [code, type], code=500"),
            "{err}"
        );
        drop(module.await.unwrap());
    }

    /// A stream whose `shutdown` never completes, like a TLS `close_notify`
    /// stuck behind a full send buffer.
    struct StuckShutdown(DuplexStream);

    impl AsyncRead for StuckShutdown {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for StuckShutdown {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    #[tokio::test]
    async fn stuck_shutdown_is_bounded() {
        let (client_end, _module_end) = tokio::io::duplex(1024);
        let mut client = SetupClient::new(StuckShutdown(client_end));
        client.shutdown_timeout = Duration::from_millis(50);
        tokio::time::timeout(Duration::from_secs(5), client.shutdown())
            .await
            .expect("shutdown must give up on its own");
    }

    #[test]
    fn shutdown_timeout_is_one_second() {
        let (client_end, _module_end) = tokio::io::duplex(8);
        let client = SetupClient::new(client_end);
        assert_eq!(client.shutdown_timeout, Duration::from_secs(1));
    }

    #[tokio::test]
    async fn read_device_info_over_tls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = tls::server_acceptor(FIXTURE.server.cert(), FIXTURE.server.key()).unwrap();
        let module = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = tls::accept(&acceptor, tcp).await.unwrap();
            let mut splitter = FrameSplitter::default();
            let mut chunk = [0u8; 4096];
            let request = loop {
                if let Some(v) = splitter.next_frame().unwrap() {
                    break v;
                }
                let n = tls.read(&mut chunk).await.unwrap();
                splitter.push(&chunk[..n]);
            };
            tls.write_all(fixture!("get_device_info_response").as_bytes())
                .await
                .unwrap();
            tls.flush().await.unwrap();
            request
        });
        let info = read_device_info("127.0.0.1", port, false).await.unwrap();
        assert_eq!(info.model_name, "RAC_000000_WW");
        let request = module.await.unwrap();
        assert_eq!(request["cmd"], "getDeviceInfo");
        let data = request["data"].as_object().unwrap();
        assert_eq!(data["constantConnect"], "Y");
        assert!(!data.contains_key("apiServer") && !data.contains_key("mqttServer"));
        assert!(
            data["publicKey"]
                .as_str()
                .unwrap()
                .starts_with("-----BEGIN PUBLIC KEY-----")
        );
    }
}
