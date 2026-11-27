//! RAC TLV codec (T1.6): the hex payloads of `device_packet` (state from the
//! unit) and `packet` (commands to it) for the `ThinQ2` `RAC_056905_WW` class.
//!
//! Clean-room, from the rethink wiki (`TLVProtocol`, `Appliance:RAC_056905_WW`,
//! `Thinq2CloudProtocol`) and rethink PR #340/#349 text. Four layers, all pure:
//!
//! 1. **MQTT prefix.** Device-to-cloud data is `[2-byte prefix][UART frame]`;
//!    prefix `0000` is a UART packet, anything else came from the Wi-Fi
//!    modem itself. Cloud-to-device data is `[a][s][UART frame]` with
//!    `a` = reliable delivery and `s` = forward the ACK ([`COMMAND_PREFIX`]).
//! 2. **UART frame** ([`Frame`]): `04 00 00 00 <dir> <b5> <b6> <seq> <len>
//!    <payload> <crc16-xmodem, big-endian>`.
//! 3. **TLV** ([`Tlv`]): a big-endian `u16` header = type (10 bits) | length
//!    (2 bits) | inline value (4 bits); length 0 means the value is the
//!    inline nibble, otherwise it is the next 1–3 bytes.
//! 4. **Typed layer**: [`AcState`] (merged from reports) and [`AcCommand`]
//!    (encoded by [`encode`] for `MqttBroker::send_packet`).
//!
//! This unit is cooling-only: Auto is mode 3 (PR #340) and there is no heat.
//! The 18–30 °C range and the fan names are inferred, unverified until T1.9.
//!
//! Logs name TLV type ids only, never values or payload hex.

use std::fmt;
use std::str::FromStr;

use crate::error::{ControlError, Result};

/// TLV type ids for this AC class (rethink wiki `Appliance:RAC_056905_WW`).
pub mod field {
    /// Query group selector (commands only): 1 = static, 2 = state.
    pub const QUERY: u16 = 0x1f5;
    /// Power: 0 off, 1 on.
    pub const POWER: u16 = 0x1f7;
    /// Operating mode, see [`super::AcMode`].
    pub const MODE: u16 = 0x1f9;
    /// Fan speed, see [`super::FanSpeed`].
    pub const FAN: u16 = 0x1fa;
    /// Room temperature, half degrees Celsius.
    pub const ROOM_TEMP: u16 = 0x1fd;
    /// Target temperature (half degrees) or, in Auto, the comfort offset.
    pub const SETPOINT: u16 = 0x1fe;
    /// Display light: 0 off, 1 on.
    pub const LIGHT: u16 = 0x21f;
}

// ---------------------------------------------------------------------------
// CRC
// ---------------------------------------------------------------------------

/// CRC-16/XMODEM (poly 0x1021, init 0, no reflection, no final xor).
#[must_use]
pub fn crc16_xmodem(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in bytes {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            };
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// UART frame
// ---------------------------------------------------------------------------

/// First byte of the UART frames we handle (`A5`, "EO packet", is not).
const FRAME_START: u8 = 0x04;
/// Start byte, three zero bytes, direction, b5, b6, seq, length.
const HEADER_LEN: usize = 9;
const CRC_LEN: usize = 2;
const MIN_FRAME_LEN: usize = HEADER_LEN + CRC_LEN;
/// `b6` of an ACK frame (empty payload, mirrors b5 and seq).
const B6_ACK: u8 = 0x10;
/// `b6` of a command frame we send.
const B6_COMMAND: u8 = 0x01;
/// `b6` of a query frame we send.
const B6_QUERY: u8 = 0x02;
/// `b5` of the frames we send (wiki command and query headers).
const B5_SEND: u8 = 0x02;

/// Who a UART frame is for (byte 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `65`: modem to appliance (our commands).
    ToAppliance,
    /// `87`: appliance to modem (state reports, ACKs).
    FromAppliance,
}

impl Direction {
    const fn byte(self) -> u8 {
        match self {
            Self::ToAppliance => 0x65,
            Self::FromAppliance => 0x87,
        }
    }

    const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x65 => Some(Self::ToAppliance),
            0x87 => Some(Self::FromAppliance),
            _ => None,
        }
    }
}

/// UART frame header bytes 4–7. `b5`/`b6` are only partly understood (wiki).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub direction: Direction,
    pub b5: u8,
    pub b6: u8,
    pub seq: u8,
}

/// A UART frame whose length and CRC checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Parses one UART frame (without MQTT prefix).
    ///
    /// # Errors
    /// [`ControlError::Device`] if it is too short, has an unsupported start
    /// or direction byte, its length byte disagrees with its size, or the
    /// CRC does not match.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < MIN_FRAME_LEN {
            return Err(ControlError::device(format!(
                "UART frame too short: {} bytes, need at least {MIN_FRAME_LEN}",
                bytes.len()
            )));
        }
        if bytes[0] != FRAME_START {
            return Err(ControlError::device(
                "UART frame has an unsupported start byte",
            ));
        }
        let Some(direction) = Direction::from_byte(bytes[4]) else {
            return Err(ControlError::device(
                "UART frame has an unknown direction byte",
            ));
        };
        let len = usize::from(bytes[8]);
        if bytes.len() != HEADER_LEN + len + CRC_LEN {
            return Err(ControlError::device(format!(
                "UART frame length mismatch: header says {len} payload bytes, frame has {}",
                bytes.len().saturating_sub(MIN_FRAME_LEN)
            )));
        }
        let (body, crc) = bytes.split_at(bytes.len() - CRC_LEN);
        if crc16_xmodem(body) != u16::from_be_bytes([crc[0], crc[1]]) {
            return Err(ControlError::device(format!(
                "UART frame CRC mismatch ({} bytes)",
                bytes.len()
            )));
        }
        Ok(Self {
            header: Header {
                direction,
                b5: bytes[5],
                b6: bytes[6],
                seq: bytes[7],
            },
            payload: body[HEADER_LEN..].to_vec(),
        })
    }

    /// Serialises the frame, computing the length byte and CRC.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if the payload is longer than 255 bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let Ok(len) = u8::try_from(self.payload.len()) else {
            return Err(ControlError::invalid(format!(
                "UART payload too long: {} bytes, at most 255",
                self.payload.len()
            )));
        };
        let h = self.header;
        let mut out = Vec::with_capacity(MIN_FRAME_LEN + self.payload.len());
        out.extend_from_slice(&[
            FRAME_START,
            0,
            0,
            0,
            h.direction.byte(),
            h.b5,
            h.b6,
            h.seq,
            len,
        ]);
        out.extend_from_slice(&self.payload);
        out.extend_from_slice(&crc16_xmodem(&out).to_be_bytes());
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// TLV
// ---------------------------------------------------------------------------

/// One TLV field. `decode(encode(t)) == t` for every encodable value; bytes
/// round-trip when the sender used the smallest length (as LG's do in every
/// capture so far).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tlv {
    /// Type id, 10 bits (the wiki text says 12; its examples show 10).
    pub kind: u16,
    /// Value, at most 24 bits.
    pub value: u32,
}

impl fmt::Debug for Tlv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Tlv {{ kind: {:#05x}, value: {} }}",
            self.kind, self.value
        )
    }
}

/// Largest TLV type id (10 bits).
pub const MAX_KIND: u16 = 0x3ff;
/// Largest TLV value (3 bytes).
pub const MAX_VALUE: u32 = 0x00ff_ffff;

/// Decodes a UART payload into TLVs, in order.
///
/// # Errors
/// [`ControlError::Device`] if a TLV is truncated.
pub fn decode_tlvs(payload: &[u8]) -> Result<Vec<Tlv>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < payload.len() {
        let start = i;
        let truncated = |need: usize| {
            ControlError::device(format!(
                "truncated TLV at offset {start}: needs {need} bytes, {} left",
                payload.len() - start
            ))
        };
        let Some(&[hi, lo]) = payload.get(i..i + 2) else {
            return Err(truncated(2));
        };
        let header = u16::from_be_bytes([hi, lo]);
        let kind = header >> 6;
        let n = usize::from((header >> 4) & 0x3);
        i += 2;
        let value = if n == 0 {
            u32::from(header & 0xf)
        } else {
            let Some(bytes) = payload.get(i..i + n) else {
                return Err(truncated(2 + n));
            };
            i += n;
            bytes.iter().fold(0, |acc, &b| (acc << 8) | u32::from(b))
        };
        out.push(Tlv { kind, value });
    }
    Ok(out)
}

/// Encodes TLVs with the smallest length for each value (inline when ≤ 15).
///
/// # Errors
/// [`ControlError::Invalid`] if a type exceeds [`MAX_KIND`] or a value
/// exceeds [`MAX_VALUE`].
pub fn encode_tlvs(tlvs: &[Tlv]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(tlvs.len() * 3);
    for t in tlvs {
        if t.kind > MAX_KIND {
            return Err(ControlError::invalid(format!(
                "TLV type {:#x} does not fit in 10 bits",
                t.kind
            )));
        }
        let n: u16 = match t.value {
            0..=0xf => 0,
            0x10..=0xff => 1,
            0x100..=0xffff => 2,
            0x1_0000..=MAX_VALUE => 3,
            _ => {
                return Err(ControlError::invalid(format!(
                    "TLV value for type {:#x} does not fit in 24 bits",
                    t.kind
                )));
            }
        };
        let be = t.value.to_be_bytes();
        let inline = if n == 0 { u16::from(be[3]) } else { 0 };
        out.extend_from_slice(&((t.kind << 6) | (n << 4) | inline).to_be_bytes());
        out.extend_from_slice(&be[4 - usize::from(n)..]);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// MQTT prefix
// ---------------------------------------------------------------------------

/// Device-to-cloud prefix of a UART packet.
pub const UART_PREFIX: [u8; 2] = [0x00, 0x00];
/// Cloud-to-device prefix we send: reliable delivery (`a` = 01) and forward
/// the ACK to us (`s` = 01), as in the wiki command and query headers.
pub const COMMAND_PREFIX: [u8; 2] = [0x01, 0x01];

/// One decoded `device_packet`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePacket {
    /// A TLV report from the unit (`b6` = 0x04 reports are partial).
    Data { header: Header, tlvs: Vec<Tlv> },
    /// The unit acknowledged frame `seq`.
    Ack { seq: u8 },
    /// A valid UART frame in a format we don't parse (`b5` not 1 or 2:
    /// the wiki's "superset" / "extended TLV" paths).
    Unsupported { header: Header, len: usize },
    /// A message the Wi-Fi modem generated itself (non-zero prefix and no
    /// valid UART frame); purpose unknown.
    Modem { prefix: [u8; 2], len: usize },
}

/// Decodes the `data` hex of a `device_packet`.
///
/// # Errors
/// [`ControlError::Device`] for bad hex, fewer than two bytes, or (with
/// prefix `0000`) a bad UART frame or truncated TLV.
pub fn decode_device_packet(hex: &str) -> Result<DevicePacket> {
    let bytes = decode_hex(hex)?;
    let Some((prefix, rest)) = bytes.split_first_chunk::<2>() else {
        return Err(ControlError::device(format!(
            "device packet too short: {} bytes",
            bytes.len()
        )));
    };
    let frame = if *prefix == UART_PREFIX {
        Frame::parse(rest)?
    } else {
        match Frame::parse(rest) {
            Ok(frame) => frame,
            Err(_) => {
                return Ok(DevicePacket::Modem {
                    prefix: *prefix,
                    len: rest.len(),
                });
            }
        }
    };
    let header = frame.header;
    if !matches!(header.b5, 0x01 | 0x02) {
        return Ok(DevicePacket::Unsupported {
            header,
            len: frame.payload.len(),
        });
    }
    if header.b6 == B6_ACK {
        return Ok(DevicePacket::Ack { seq: header.seq });
    }
    Ok(DevicePacket::Data {
        header,
        tlvs: decode_tlvs(&frame.payload)?,
    })
}

/// Splits a cloud-to-device `packet` hex into its prefix and UART frame
/// (the inverse of [`encode`], for tests and diagnostics).
///
/// # Errors
/// [`ControlError::Device`] for bad hex, fewer than two bytes or a bad frame.
pub fn decode_command_packet(hex: &str) -> Result<([u8; 2], Frame)> {
    let bytes = decode_hex(hex)?;
    let Some((prefix, rest)) = bytes.split_first_chunk::<2>() else {
        return Err(ControlError::device(format!(
            "command packet too short: {} bytes",
            bytes.len()
        )));
    };
    Ok((*prefix, Frame::parse(rest)?))
}

fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    hex::decode(hex).map_err(|e| match e {
        hex::FromHexError::InvalidHexCharacter { index, .. } => ControlError::device(format!(
            "packet is not valid hex: bad character at offset {index}"
        )),
        hex::FromHexError::OddLength | hex::FromHexError::InvalidStringLength => {
            ControlError::device(format!(
                "packet is not valid hex: odd length ({} chars)",
                hex.len()
            ))
        }
    })
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

const HEAT_UNSUPPORTED: &str =
    "this AC is cooling-only: heat is not supported (supported: cool, dry, fan, auto)";

/// Operating mode of this cooling-only unit (0x1f9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcMode {
    Cool,
    Dry,
    Fan,
    /// LG: "AI". Code 3 on cooling-only units (PR #340); 6 on heat pumps.
    Auto,
}

impl AcMode {
    /// The 0x1f9 value.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Cool => 0,
            Self::Dry => 1,
            Self::Fan => 2,
            Self::Auto => 3,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cool => "cool",
            Self::Dry => "dry",
            Self::Fan => "fan",
            Self::Auto => "auto",
        }
    }
}

impl TryFrom<u8> for AcMode {
    type Error = ControlError;

    fn try_from(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Cool),
            1 => Ok(Self::Dry),
            2 => Ok(Self::Fan),
            3 => Ok(Self::Auto),
            // 4 = heat, 6 = heat-pump auto (wiki): neither exists here.
            4 | 6 => Err(ControlError::invalid(HEAT_UNSUPPORTED)),
            _ => Err(ControlError::invalid(format!(
                "unknown AC mode code {code} (supported: cool 0, dry 1, fan 2, auto 3)"
            ))),
        }
    }
}

impl FromStr for AcMode {
    type Err = ControlError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cool" => Ok(Self::Cool),
            "dry" => Ok(Self::Dry),
            "fan" => Ok(Self::Fan),
            "auto" => Ok(Self::Auto),
            "heat" | "auto6" => Err(ControlError::invalid(HEAT_UNSUPPORTED)),
            _ => Err(ControlError::invalid(format!(
                "unknown AC mode {s:?} (supported: cool, dry, fan, auto)"
            ))),
        }
    }
}

impl fmt::Display for AcMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fan speed (0x1fa): 2–6 are the remote's bars, 8 is auto. Names inferred,
/// unverified until T1.9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanSpeed {
    Low,
    LowMid,
    Mid,
    MidHigh,
    High,
    Auto,
}

impl FanSpeed {
    /// The 0x1fa value.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Low => 2,
            Self::LowMid => 3,
            Self::Mid => 4,
            Self::MidHigh => 5,
            Self::High => 6,
            Self::Auto => 8,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::LowMid => "low-mid",
            Self::Mid => "mid",
            Self::MidHigh => "mid-high",
            Self::High => "high",
            Self::Auto => "auto",
        }
    }
}

impl TryFrom<u8> for FanSpeed {
    type Error = ControlError;

    fn try_from(code: u8) -> Result<Self> {
        match code {
            2 => Ok(Self::Low),
            3 => Ok(Self::LowMid),
            4 => Ok(Self::Mid),
            5 => Ok(Self::MidHigh),
            6 => Ok(Self::High),
            8 => Ok(Self::Auto),
            _ => Err(ControlError::invalid(format!(
                "unknown fan speed code {code} (supported: 2-6, auto 8)"
            ))),
        }
    }
}

impl FromStr for FanSpeed {
    type Err = ControlError;

    fn from_str(s: &str) -> Result<Self> {
        let norm: String = s
            .chars()
            .filter(|c| !matches!(c, '-' | '_'))
            .map(|c| c.to_ascii_lowercase())
            .collect();
        match norm.as_str() {
            "low" => Ok(Self::Low),
            "lowmid" => Ok(Self::LowMid),
            "mid" => Ok(Self::Mid),
            "midhigh" => Ok(Self::MidHigh),
            "high" => Ok(Self::High),
            "auto" => Ok(Self::Auto),
            _ => Err(ControlError::invalid(format!(
                "unknown fan speed {s:?} (supported: low, low-mid, mid, mid-high, high, auto)"
            ))),
        }
    }
}

impl fmt::Display for FanSpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Target temperature in half degrees Celsius (0x1fe outside Auto).
///
/// Input is checked to 18.0–30.0 °C in 0.5 °C steps (range inferred,
/// unverified until T1.9); values reported by the unit are taken as is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TargetTemp(u8);

impl TargetTemp {
    /// 18.0 °C.
    pub const MIN_HALF_DEGREES: u8 = 36;
    /// 30.0 °C.
    pub const MAX_HALF_DEGREES: u8 = 60;

    /// # Errors
    /// [`ControlError::Invalid`] outside 18.0–30.0 °C or off the 0.5 °C step.
    pub fn from_celsius(celsius: f32) -> Result<Self> {
        let half = celsius * 2.0;
        if let Some(h) = (Self::MIN_HALF_DEGREES..=Self::MAX_HALF_DEGREES)
            .find(|h| (f32::from(*h) - half).abs() < 0.01)
        {
            return Ok(Self(h));
        }
        if (18.0..=30.0).contains(&celsius) {
            Err(ControlError::invalid(format!(
                "target temperature must be a multiple of 0.5 °C (got {celsius})"
            )))
        } else {
            Err(ControlError::invalid(format!(
                "target temperature must be 18.0–30.0 °C (got {celsius})"
            )))
        }
    }

    /// # Errors
    /// [`ControlError::Invalid`] outside 36..=60 (18.0–30.0 °C).
    pub fn from_half_degrees(half: u8) -> Result<Self> {
        if (Self::MIN_HALF_DEGREES..=Self::MAX_HALF_DEGREES).contains(&half) {
            Ok(Self(half))
        } else {
            Err(ControlError::invalid(format!(
                "target temperature must be 18.0–30.0 °C (got {} °C)",
                f32::from(half) / 2.0
            )))
        }
    }

    #[must_use]
    pub const fn half_degrees(self) -> u8 {
        self.0
    }

    #[must_use]
    pub fn celsius(self) -> f32 {
        f32::from(self.0) / 2.0
    }
}

/// Room temperature in half degrees Celsius (0x1fd), as reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RoomTemp(u8);

impl RoomTemp {
    #[must_use]
    pub const fn half_degrees(self) -> u8 {
        self.0
    }

    #[must_use]
    pub fn celsius(self) -> f32 {
        f32::from(self.0) / 2.0
    }
}

/// Auto-mode comfort offset, −2..=+2 steps; sent in 0x1fe as `34 + 2·offset`
/// (PR #340 text; unverified until T1.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComfortOffset(i8);

impl ComfortOffset {
    const BASE: u8 = 34;

    /// # Errors
    /// [`ControlError::Invalid`] outside −2..=+2.
    pub fn new(offset: i8) -> Result<Self> {
        if (-2..=2).contains(&offset) {
            Ok(Self(offset))
        } else {
            Err(ControlError::invalid(format!(
                "comfort offset must be -2..=+2 (got {offset})"
            )))
        }
    }

    /// The offset reported in 0x1fe, if `raw` is one of 30, 32, 34, 36, 38.
    #[must_use]
    pub fn from_raw(raw: u8) -> Option<Self> {
        if !raw.is_multiple_of(2) {
            return None;
        }
        let offset = i8::try_from((i16::from(raw) - i16::from(Self::BASE)) / 2).ok()?;
        Self::new(offset).ok()
    }

    #[must_use]
    pub const fn get(self) -> i8 {
        self.0
    }

    /// The 0x1fe value.
    #[must_use]
    pub const fn raw(self) -> u8 {
        let step = 2 * self.0.unsigned_abs();
        if self.0 >= 0 {
            Self::BASE + step
        } else {
            Self::BASE - step
        }
    }
}

/// What 0x1fe carries: a temperature, or the comfort offset in Auto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setpoint {
    Temp(TargetTemp),
    Offset(ComfortOffset),
}

impl Setpoint {
    const fn raw(self) -> u8 {
        match self {
            Self::Temp(t) => t.half_degrees(),
            Self::Offset(o) => o.raw(),
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// The unit's state as far as reports have told us. Reports may be partial,
/// so [`AcState::apply`] merges them; `None` means "not reported yet" (or
/// last reported with a value we don't model, see `unknown`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcState {
    pub power: Option<bool>,
    pub mode: Option<AcMode>,
    pub fan: Option<FanSpeed>,
    pub room_temp: Option<RoomTemp>,
    /// Raw 0x1fe: read it through [`Self::target_temp`] or
    /// [`Self::comfort_offset`], which depend on the mode.
    pub setpoint_raw: Option<u8>,
    pub light: Option<bool>,
    /// Fields we don't model, plus known fields with a value we can't
    /// interpret; the latest report of each type wins.
    pub unknown: Vec<Tlv>,
}

impl AcState {
    /// Merges one report. For every type in `tlvs`, older `unknown` entries
    /// of that type are dropped; new unknown fields are appended in order.
    pub fn apply(&mut self, tlvs: &[Tlv]) {
        let mut fresh = Vec::new();
        for &t in tlvs {
            if !self.apply_known(t) {
                fresh.push(t);
            }
        }
        self.unknown
            .retain(|old| !tlvs.iter().any(|t| t.kind == old.kind));
        if !fresh.is_empty() {
            let types: Vec<String> = fresh.iter().map(|t| format!("{:#05x}", t.kind)).collect();
            tracing::debug!(
                count = fresh.len(),
                types = %types.join(","),
                "ac tlv: kept unknown fields"
            );
        }
        self.unknown.extend(fresh);
    }

    /// Applies a modelled field; `false` if `t` belongs in `unknown`.
    fn apply_known(&mut self, t: Tlv) -> bool {
        let byte = u8::try_from(t.value).ok();
        let ok = match t.kind {
            field::POWER => {
                self.power = flag(t.value);
                self.power.is_some()
            }
            field::MODE => {
                self.mode = byte.and_then(|c| AcMode::try_from(c).ok());
                self.mode.is_some()
            }
            field::FAN => {
                self.fan = byte.and_then(|c| FanSpeed::try_from(c).ok());
                self.fan.is_some()
            }
            field::ROOM_TEMP => {
                self.room_temp = byte.map(RoomTemp);
                self.room_temp.is_some()
            }
            field::SETPOINT => {
                self.setpoint_raw = byte;
                self.setpoint_raw.is_some()
            }
            field::LIGHT => {
                self.light = flag(t.value);
                self.light.is_some()
            }
            _ => return false,
        };
        if !ok {
            tracing::warn!(
                kind = %format_args!("{:#05x}", t.kind),
                "ac tlv: known field with an unsupported value, kept as unknown"
            );
        }
        ok
    }

    /// The target temperature, unless the mode is Auto or not known yet.
    #[must_use]
    pub fn target_temp(&self) -> Option<TargetTemp> {
        match self.mode? {
            AcMode::Auto => None,
            _ => self.setpoint_raw.map(TargetTemp),
        }
    }

    /// The comfort offset, only in Auto.
    #[must_use]
    pub fn comfort_offset(&self) -> Option<ComfortOffset> {
        match self.mode? {
            AcMode::Auto => self.setpoint_raw.and_then(ComfortOffset::from_raw),
            _ => None,
        }
    }
}

const fn flag(value: u32) -> Option<bool> {
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Which group a state query asks for (0x1f5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryGroup {
    /// 1: a static-looking group (capabilities).
    Static,
    /// 2: the variable state (power, mode, fan, temperatures, swing).
    State,
}

/// A command to the unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcCommand {
    /// 0x1f7 = 0, sent alone: with other fields the unit plays the shutdown
    /// melody and keeps running (wiki).
    PowerOff,
    /// 0x1f7 = 1, sent alone.
    PowerOn,
    /// Mode, fan and setpoint always go together: the unit ignores a
    /// temperature sent without mode and fan (wiki).
    Set {
        mode: AcMode,
        fan: FanSpeed,
        /// [`Setpoint::Offset`] in Auto, [`Setpoint::Temp`] otherwise.
        setpoint: Setpoint,
    },
    /// Display light (0x21f).
    Light(bool),
    /// State query (0x1f5).
    Query(QueryGroup),
}

impl AcCommand {
    /// The TLVs this command sends, in order.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if a `Set` setpoint doesn't match its mode.
    pub fn tlvs(&self) -> Result<Vec<Tlv>> {
        let tlv = |kind, value: u8| Tlv {
            kind,
            value: u32::from(value),
        };
        Ok(match self {
            Self::PowerOff => vec![tlv(field::POWER, 0)],
            Self::PowerOn => vec![tlv(field::POWER, 1)],
            Self::Set {
                mode,
                fan,
                setpoint,
            } => {
                match (mode, setpoint) {
                    (AcMode::Auto, Setpoint::Temp(_)) => {
                        return Err(ControlError::invalid(
                            "auto mode takes a comfort offset (-2..=+2), not a target temperature",
                        ));
                    }
                    (m, Setpoint::Offset(_)) if *m != AcMode::Auto => {
                        return Err(ControlError::invalid(format!(
                            "{m} mode takes a target temperature, not a comfort offset"
                        )));
                    }
                    _ => {}
                }
                vec![
                    tlv(field::MODE, mode.code()),
                    tlv(field::FAN, fan.code()),
                    tlv(field::SETPOINT, setpoint.raw()),
                ]
            }
            Self::Light(on) => vec![tlv(field::LIGHT, u8::from(*on))],
            Self::Query(group) => vec![tlv(
                field::QUERY,
                match group {
                    QueryGroup::Static => 1,
                    QueryGroup::State => 2,
                },
            )],
        })
    }
}

/// Encodes `cmd` as the lowercase `packet` hex for `MqttBroker::send_packet`:
/// [`COMMAND_PREFIX`], then a UART frame with header `65 02 01 <seq>`
/// (commands) or `65 02 02 <seq>` (queries).
///
/// # Errors
/// [`ControlError::Invalid`] if the command is inconsistent (see
/// [`AcCommand::tlvs`]).
pub fn encode(cmd: &AcCommand, seq: u8) -> Result<String> {
    let b6 = match cmd {
        AcCommand::Query(_) => B6_QUERY,
        _ => B6_COMMAND,
    };
    let frame = Frame {
        header: Header {
            direction: Direction::ToAppliance,
            b5: B5_SEND,
            b6,
            seq,
        },
        payload: encode_tlvs(&cmd.tlvs()?)?,
    };
    let mut out = COMMAND_PREFIX.to_vec();
    out.extend(frame.to_bytes()?);
    Ok(hex::encode(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/ac_tlv/",
                $name,
                ".json"
            ))
            .as_slice()
        };
    }

    const FAKE_DID: &str = "00000000-fake-0000-0000-000000000000";

    fn t(kind: u16, value: u32) -> Tlv {
        Tlv { kind, value }
    }

    /// The `data` hex of a `device_packet` fixture (checks the fake did).
    fn data(bytes: &[u8]) -> String {
        let m: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(m["did"], FAKE_DID);
        assert_eq!(m["cmd"], "device_packet");
        m["data"].as_str().unwrap().to_owned()
    }

    /// Re-encodes a device packet from its parts and checks it is identical.
    fn assert_device_round_trip(hex: &str) {
        let bytes = hex::decode(hex).unwrap();
        let frame = Frame::parse(&bytes[2..]).unwrap();
        let rebuilt = Frame {
            header: frame.header,
            payload: encode_tlvs(&decode_tlvs(&frame.payload).unwrap()).unwrap(),
        };
        let mut out = bytes[..2].to_vec();
        out.extend(rebuilt.to_bytes().unwrap());
        assert_eq!(hex::encode(out), hex.to_ascii_lowercase());
    }

    fn message(r: Result<impl fmt::Debug>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn crc_matches_xmodem_check_value() {
        assert_eq!(crc16_xmodem(b"123456789"), 0x31c3);
    }

    #[test]
    fn wiki_frames_decode() {
        // TLVProtocol example frame (modem to appliance).
        let bytes = hex::decode("040000006501010102AA43E6CD").unwrap();
        let f = Frame::parse(&bytes).unwrap();
        assert_eq!(
            f.header,
            Header {
                direction: Direction::ToAppliance,
                b5: 1,
                b6: 1,
                seq: 1
            }
        );
        assert_eq!(decode_tlvs(&f.payload).unwrap(), vec![t(0x2a9, 3)]);
        assert_eq!(f.to_bytes().unwrap(), bytes);

        // Unsolicited partial report.
        let hex = "000004000000870204690B8CA036458CD059ACE001DE6AC9";
        let DevicePacket::Data { header, tlvs } = decode_device_packet(hex).unwrap() else {
            panic!("expected data");
        };
        assert_eq!((header.b5, header.b6, header.seq), (2, 4, 0x69));
        assert_eq!(
            tlvs,
            vec![t(0x232, 0x3645), t(0x233, 0x59), t(0x2b3, 0x1de)]
        );
        assert_device_round_trip(hex);

        // Static query, sent without reliable delivery (a = 00).
        let (prefix, f) = decode_command_packet("00010400000065020201027D416A0D").unwrap();
        assert_eq!(prefix, [0, 1]);
        assert_eq!((f.header.b5, f.header.b6, f.header.seq), (2, 2, 1));
        assert_eq!(decode_tlvs(&f.payload).unwrap(), vec![t(field::QUERY, 1)]);

        // ACK behind a modem prefix: still a UART frame since its CRC checks.
        assert_eq!(
            decode_device_packet("0301040000008701100000EC3C").unwrap(),
            DevicePacket::Ack { seq: 0 }
        );
    }

    #[test]
    fn annotated_tlvs_round_trip() {
        let cases = [
            ("B001", 704, 1),
            ("B05057", 705, 0x57),
            ("B0A0017C", 706, 380),
            ("B6F0690409", 731, 6_882_313),
            ("AA43", 0x2a9, 3),
            ("8CA03645", 0x232, 0x3645),
            ("8CD059", 0x233, 0x59),
            ("ACE001DE", 0x2b3, 0x1de),
        ];
        for (hex, kind, value) in cases {
            let bytes = hex::decode(hex).unwrap();
            assert_eq!(decode_tlvs(&bytes).unwrap(), vec![t(kind, value)], "{hex}");
            assert_eq!(encode_tlvs(&[t(kind, value)]).unwrap(), bytes, "{hex}");
        }
    }

    #[test]
    fn encoder_picks_smallest_length_and_rejects_overflow() {
        let len = |v| encode_tlvs(&[t(0x1f7, v)]).unwrap().len();
        assert_eq!(
            [
                len(0),
                len(15),
                len(16),
                len(0xff),
                len(0x100),
                len(0xffff),
                len(0x1_0000),
                len(MAX_VALUE)
            ],
            [2, 2, 3, 3, 4, 4, 5, 5]
        );
        assert!(message(encode_tlvs(&[t(1, MAX_VALUE + 1)])).contains("24 bits"));
        assert!(message(encode_tlvs(&[t(MAX_KIND + 1, 0)])).contains("10 bits"));
        for v in [0, 9, 0x10, 0xabc, 0x12_3456, MAX_VALUE] {
            let tl = [t(MAX_KIND, v), t(0, v)];
            assert_eq!(decode_tlvs(&encode_tlvs(&tl).unwrap()).unwrap(), tl);
        }
    }

    #[test]
    fn state_report_fixture() {
        let hex = data(fixture!("state_report"));
        let DevicePacket::Data { header, tlvs } = decode_device_packet(&hex).unwrap() else {
            panic!("expected data");
        };
        assert_eq!(header.direction, Direction::FromAppliance);
        let mut state = AcState::default();
        state.apply(&tlvs);
        assert_eq!(state.target_temp().map(TargetTemp::celsius), Some(24.0));
        assert_eq!(state.room_temp.map(RoomTemp::celsius), Some(21.5));
        assert_eq!(state.comfort_offset(), None);
        insta::assert_debug_snapshot!("state_report", state);
        assert_device_round_trip(&hex);
    }

    #[test]
    fn partial_report_merges_and_replaces_unknown() {
        let mut state = AcState::default();
        for name in [fixture!("state_report"), fixture!("partial_report")] {
            let DevicePacket::Data { tlvs, .. } = decode_device_packet(&data(name)).unwrap() else {
                panic!("expected data");
            };
            state.apply(&tlvs);
        }
        // Mode/setpoint changed, the rest kept; 0x321 replaced, not duplicated.
        assert_eq!(state.mode, Some(AcMode::Auto));
        assert_eq!(state.power, Some(true));
        assert_eq!(state.target_temp(), None);
        assert_eq!(state.comfort_offset().map(ComfortOffset::get), Some(1));
        assert_eq!(state.unknown.iter().filter(|u| u.kind == 0x321).count(), 1);
        insta::assert_debug_snapshot!("partial_report_merged", state);
        assert_device_round_trip(&data(fixture!("partial_report")));
    }

    #[test]
    fn ack_fixture() {
        let hex = data(fixture!("ack"));
        assert_eq!(
            decode_device_packet(&hex).unwrap(),
            DevicePacket::Ack { seq: 3 }
        );
        assert_device_round_trip(&hex);
    }

    #[test]
    fn command_fixtures_round_trip() {
        let offset = |o| Setpoint::Offset(ComfortOffset::new(o).unwrap());
        let temp = |c| Setpoint::Temp(TargetTemp::from_celsius(c).unwrap());
        let cases: serde_json::Value = serde_json::from_slice(fixture!("commands")).unwrap();
        let cases = cases.as_array().unwrap();
        assert_eq!(cases.len(), 7);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let cmd = match name {
                "power_off" => AcCommand::PowerOff,
                "power_on" => AcCommand::PowerOn,
                "set_cool_mid_24" => AcCommand::Set {
                    mode: AcMode::Cool,
                    fan: FanSpeed::Mid,
                    setpoint: temp(24.0),
                },
                "set_auto_auto_minus1" => AcCommand::Set {
                    mode: AcMode::Auto,
                    fan: FanSpeed::Auto,
                    setpoint: offset(-1),
                },
                "light_off" => AcCommand::Light(false),
                "query_state" => AcCommand::Query(QueryGroup::State),
                "query_static" => AcCommand::Query(QueryGroup::Static),
                other => panic!("unknown fixture case {other}"),
            };
            let seq = u8::try_from(case["seq"].as_u64().unwrap()).unwrap();
            let hex = case["data"].as_str().unwrap();
            assert_eq!(encode(&cmd, seq).unwrap(), hex, "{name}");

            let (prefix, frame) = decode_command_packet(hex).unwrap();
            assert_eq!(prefix, COMMAND_PREFIX, "{name}");
            let want_b6 = if name.starts_with("query") { 2 } else { 1 };
            assert_eq!(
                frame.header,
                Header {
                    direction: Direction::ToAppliance,
                    b5: 2,
                    b6: want_b6,
                    seq
                },
                "{name}"
            );
            assert_eq!(
                decode_tlvs(&frame.payload).unwrap(),
                cmd.tlvs().unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn heat_is_rejected() {
        for code in [4, 6] {
            let e = message(AcMode::try_from(code));
            assert!(e.contains("cooling-only") && e.contains("heat"), "{e}");
        }
        for s in ["heat", "HEAT", "auto6"] {
            let e = message(s.parse::<AcMode>());
            assert!(e.contains("cooling-only"), "{e}");
            assert!(matches!(
                s.parse::<AcMode>(),
                Err(ControlError::Invalid { .. })
            ));
        }
        assert!(message(AcMode::try_from(5)).contains("unknown AC mode code 5"));
        for (s, m) in [
            ("cool", AcMode::Cool),
            ("Dry", AcMode::Dry),
            ("fan", AcMode::Fan),
            ("auto", AcMode::Auto),
        ] {
            assert_eq!(s.parse::<AcMode>().unwrap(), m);
            assert_eq!(AcMode::try_from(m.code()).unwrap(), m);
        }
    }

    #[test]
    fn reported_heat_mode_is_kept_as_unknown() {
        let mut state = AcState::default();
        state.apply(&[t(field::MODE, 0), t(field::SETPOINT, 48)]);
        assert_eq!(state.mode, Some(AcMode::Cool));
        state.apply(&[t(field::MODE, 4), t(field::POWER, 7)]);
        assert_eq!((state.mode, state.power), (None, None));
        assert_eq!(state.target_temp(), None);
        assert_eq!(state.unknown, vec![t(field::MODE, 4), t(field::POWER, 7)]);
        state.apply(&[t(field::MODE, 1)]);
        assert_eq!(state.mode, Some(AcMode::Dry));
        assert_eq!(state.unknown, vec![t(field::POWER, 7)]);
    }

    #[test]
    fn zero_filled_frame_decodes_to_unknown_fields() {
        // Wiki "Bugs": zeroed payload with a valid CRC parses as type-0 TLVs.
        let frame = Frame {
            header: Header {
                direction: Direction::FromAppliance,
                b5: 2,
                b6: 4,
                seq: 9,
            },
            payload: vec![0; 4],
        };
        let mut bytes = UART_PREFIX.to_vec();
        bytes.extend(frame.to_bytes().unwrap());
        let DevicePacket::Data { tlvs, .. } = decode_device_packet(&hex::encode(bytes)).unwrap()
        else {
            panic!("expected data");
        };
        let mut state = AcState::default();
        state.apply(&tlvs);
        assert_eq!(state.unknown, vec![t(0, 0), t(0, 0)]);
        assert_eq!(state.power, None);
    }

    #[test]
    fn frame_errors_are_fatal_and_name_the_problem() {
        let good = data(fixture!("state_report"));
        let mut bad_crc = good.clone();
        bad_crc.replace_range(bad_crc.len() - 1.., "0");
        assert!(message(decode_device_packet(&bad_crc)).contains("CRC mismatch"));
        assert!(message(decode_device_packet(&good[..good.len() - 2])).contains("length mismatch"));
        assert!(message(decode_device_packet("00000400")).contains("too short"));
        assert!(message(decode_device_packet("00")).contains("too short"));
        assert!(message(decode_device_packet("0000zz")).contains("offset 4"));
        assert!(message(decode_device_packet("000")).contains("odd length"));
        // Errors never echo payload hex.
        assert!(!message(decode_device_packet(&bad_crc)).contains("7dc1"));
        // Unsupported b5 ("superset" path) is not parsed as TLV.
        let mut f = Frame::parse(&hex::decode(&good).unwrap()[2..]).unwrap();
        f.header.b5 = 0x10;
        let mut bytes = UART_PREFIX.to_vec();
        bytes.extend(f.to_bytes().unwrap());
        assert!(matches!(
            decode_device_packet(&hex::encode(bytes)).unwrap(),
            DevicePacket::Unsupported { len: 23, .. }
        ));
        // A modem message without a valid frame is not an error.
        assert_eq!(
            decode_device_packet("0301deadbeef").unwrap(),
            DevicePacket::Modem {
                prefix: [3, 1],
                len: 4
            }
        );
    }

    #[test]
    fn truncated_tlv_is_fatal() {
        assert!(message(decode_tlvs(&[0x7d])).contains("truncated TLV at offset 0"));
        assert!(
            message(decode_tlvs(&[0x7d, 0xc1, 0x8c, 0xa0, 0x36]))
                .contains("offset 2: needs 4 bytes, 3 left")
        );
        let frame = Frame {
            header: Header {
                direction: Direction::FromAppliance,
                b5: 2,
                b6: 4,
                seq: 1,
            },
            payload: vec![0x7f, 0x50],
        };
        let mut bytes = UART_PREFIX.to_vec();
        bytes.extend(frame.to_bytes().unwrap());
        assert!(message(decode_device_packet(&hex::encode(bytes))).contains("truncated TLV"));
    }

    #[test]
    fn setpoint_must_match_mode() {
        let temp = Setpoint::Temp(TargetTemp::from_celsius(22.5).unwrap());
        let offset = Setpoint::Offset(ComfortOffset::new(0).unwrap());
        let auto_temp = AcCommand::Set {
            mode: AcMode::Auto,
            fan: FanSpeed::Auto,
            setpoint: temp,
        };
        assert!(message(encode(&auto_temp, 1)).contains("auto mode takes a comfort offset"));
        let cool_offset = AcCommand::Set {
            mode: AcMode::Cool,
            fan: FanSpeed::Low,
            setpoint: offset,
        };
        assert!(message(encode(&cool_offset, 1)).contains("cool mode takes a target temperature"));
    }

    #[test]
    fn temperatures_are_range_and_step_checked() {
        for c in [17.5, 30.5, -1.0, f32::NAN, f32::INFINITY] {
            let e = message(TargetTemp::from_celsius(c));
            assert!(e.contains("18.0–30.0"), "{c}: {e}");
        }
        assert!(message(TargetTemp::from_celsius(24.3)).contains("multiple of 0.5"));
        assert_eq!(TargetTemp::from_celsius(18.0).unwrap().half_degrees(), 36);
        assert_eq!(TargetTemp::from_celsius(30.0).unwrap().half_degrees(), 60);
        assert_eq!(TargetTemp::from_celsius(21.5).unwrap().half_degrees(), 43);
        assert!(TargetTemp::from_half_degrees(35).is_err());
        assert_eq!(TargetTemp::from_half_degrees(60).unwrap().celsius(), 30.0);
        assert!(message(ComfortOffset::new(3)).contains("-2..=+2"));
        let raws: Vec<u8> = (-2..=2)
            .map(|o| ComfortOffset::new(o).unwrap().raw())
            .collect();
        assert_eq!(raws, [30, 32, 34, 36, 38]);
        for r in raws {
            assert_eq!(ComfortOffset::from_raw(r).unwrap().raw(), r);
        }
        assert_eq!(ComfortOffset::from_raw(33), None);
        assert_eq!(ComfortOffset::from_raw(40), None);
    }

    #[test]
    fn fan_names_and_codes() {
        for f in [
            FanSpeed::Low,
            FanSpeed::LowMid,
            FanSpeed::Mid,
            FanSpeed::MidHigh,
            FanSpeed::High,
            FanSpeed::Auto,
        ] {
            assert_eq!(f.as_str().parse::<FanSpeed>().unwrap(), f);
            assert_eq!(FanSpeed::try_from(f.code()).unwrap(), f);
        }
        assert_eq!("low_mid".parse::<FanSpeed>().unwrap(), FanSpeed::LowMid);
        assert!(FanSpeed::try_from(7).is_err());
        assert!("turbo".parse::<FanSpeed>().is_err());
    }
}
