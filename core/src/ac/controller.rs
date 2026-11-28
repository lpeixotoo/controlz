//! AC controller (T1.9): runs the local cloud ([`CloudSession`]) and the
//! broker ([`MqttSession`]) for one provisioned module and turns their events
//! into an [`AcState`] that can be read ([`AcController::status`]) and
//! changed ([`AcController::apply`]).
//!
//! Every command carries a sequence number, but the module's ACK frame
//! always carries seq 0 (T1.9 device run), so ACKs are matched by count:
//! the unit ACKs every frame once, in order, so a command is confirmed by
//! the first ACK after those still owed for earlier frames (a query's ACK
//! comes about a second after its report; a timed-out command's may come
//! late), or by a state report after the send that shows the change, in
//! case the ACK is lost. A change is only reported done once each command was confirmed and
//! a state query brought a fresh full report, so the caller sees the unit's
//! own read-back, never an assumed state. Only a report holding the full
//! state group ([`FULL_STATE`]) answers a query: a command's partial report
//! trails its ACK, and the unit also reports single fields on its own.
//!
//! The order follows the TLV rules (KB, T1.6, T1.9): power off goes alone;
//! power on goes with mode, fan and setpoint in one frame (the unit ignores
//! it alone); mode, fan and setpoint always go together, so missing ones are
//! filled from the current state; the display light goes alone, last.
//!
//! Plain Rust API (no uniffi yet): the CLI's `ac serve` owns one controller.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use super::cloud::{API_PORT, CloudConfig, CloudProgress, CloudServer, CloudSession, MQTT_PORT};
use super::mqtt::{DeviceEvent, MqttBroker, MqttConfig, MqttSession, Pinning, ProvisioningKind};
use super::pki::Pki;
use super::tlv::{
    self, AcCommand, AcMode, AcState, ComfortOffset, DevicePacket, FanSpeed, QueryGroup, Setpoint,
    TargetTemp,
};
use crate::error::{ControlError, Result};

/// Room for [`Milestone`]s not yet read; older ones are dropped beyond it.
const MILESTONE_QUEUE: usize = 64;
/// Room for ACKs not yet read by a waiting command.
const ACK_QUEUE: usize = 32;
/// How long a frame sent to the unit is still counted as owing its ACK;
/// older ones are assumed lost (real ACKs come within about a second).
const ACK_OWED_FOR: Duration = Duration::from_secs(30);
/// The fields a report must hold to answer a state query. The real reply
/// carries them all (with 47 more, `ac_tlv/state_report_full.json`); a
/// command's partial report never holds the room temperature, and the
/// spontaneous ones hold one field.
const FULL_STATE: [u16; 5] = [
    tlv::field::POWER,
    tlv::field::MODE,
    tlv::field::FAN,
    tlv::field::SETPOINT,
    tlv::field::ROOM_TEMP,
];

/// Where the controller listens and what it tells the module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServeConfig {
    /// This Mac's LAN address (never the unspecified address).
    pub bind: IpAddr,
    /// The host advertised in `/route` (IP or DNS name); the server
    /// certificate must carry it as a SAN.
    pub host: String,
    /// HTTPS port: [`API_PORT`] in production, `0` in tests.
    pub https_port: u16,
    /// MQTTS port: [`MQTT_PORT`] in production, `0` in tests.
    pub mqtt_port: u16,
    /// How the broker accepts a module certificate we did not issue (D21);
    /// `None` refuses it.
    pub pinning: Option<Pinning>,
}

impl ServeConfig {
    /// The production ports (443 and 8883), which the module expects.
    #[must_use]
    pub fn new(bind: IpAddr, host: impl Into<String>) -> Self {
        Self {
            bind,
            host: host.into(),
            https_port: API_PORT,
            mqtt_port: MQTT_PORT,
            pinning: None,
        }
    }
}

/// How long to wait for the unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Timeouts {
    /// For the confirmation (ACK or matching report) of one command.
    pub(crate) ack: Duration,
    /// For a state report after a query.
    pub(crate) report: Duration,
}

impl Timeouts {
    pub(crate) const PRODUCTION: Self = Self {
        ack: Duration::from_secs(5),
        report: Duration::from_secs(10),
    };
}

/// Progress of the module through provisioning and control, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Milestone {
    /// The module fetched `/route`.
    RouteServed,
    /// The module fetched our CA from `/route/certificate`.
    CaServed,
    /// The module's CSR was signed.
    ClientCertIssued,
    /// The module's MQTT CONNECT was accepted.
    MqttConnected,
    /// The module acknowledged `completeProvisioning`.
    Provisioned(ProvisioningKind),
    /// The MQTT session ended.
    MqttDisconnected,
    /// A state report changed the known state (the new state).
    State(AcState),
}

/// A snapshot of what the controller knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcStatus {
    /// The merged state from every report so far.
    pub state: AcState,
    /// Whether the module has an MQTT session with us.
    pub connected: bool,
    /// Whether that session completed provisioning (`completeProvisioning_ack`).
    pub provisioned: bool,
    /// Time since the last state report, if any arrived.
    pub last_report_age: Option<Duration>,
}

/// What to change; `None` keeps the current value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AcChange {
    /// Power off is sent alone; power on is sent with mode, fan and
    /// setpoint (from the change, else the current state) in one frame.
    pub power: Option<bool>,
    /// Operating mode; sent with fan and setpoint in one `Set`.
    pub mode: Option<AcMode>,
    /// Target temperature (not in Auto).
    pub temp: Option<TargetTemp>,
    /// Fan speed; sent with mode and setpoint in one `Set`.
    pub fan: Option<FanSpeed>,
    /// Comfort offset (Auto only).
    pub offset: Option<ComfortOffset>,
    /// Display light; sent alone, last.
    pub light: Option<bool>,
}

impl AcChange {
    /// Whether mode, fan or setpoint change (one `Set` command).
    #[must_use]
    pub fn touches_set(&self) -> bool {
        self.mode.is_some() || self.temp.is_some() || self.fan.is_some() || self.offset.is_some()
    }

    /// Rejects combinations that are wrong whatever the unit's state.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] if nothing changes, `power off` comes with
    /// mode/fan/setpoint (the unit's response to that is unknown), both a
    /// temperature and an offset are given, or the mode given does not take
    /// the setpoint given.
    pub fn check(&self) -> Result<()> {
        if !self.touches_set() && self.power.is_none() && self.light.is_none() {
            return Err(ControlError::invalid("nothing to change"));
        }
        if self.power == Some(false) && self.touches_set() {
            return Err(ControlError::invalid(
                "power off cannot be combined with mode, temperature, fan or offset",
            ));
        }
        if self.temp.is_some() && self.offset.is_some() {
            return Err(ControlError::invalid(
                "give a temperature or a comfort offset, not both",
            ));
        }
        match self.mode {
            Some(AcMode::Auto) if self.temp.is_some() => Err(auto_takes_offset()),
            Some(mode) if mode != AcMode::Auto && self.offset.is_some() => {
                Err(mode_takes_temp(mode))
            }
            _ => Ok(()),
        }
    }
}

fn auto_takes_offset() -> ControlError {
    ControlError::invalid("auto mode takes a comfort offset (-2..=+2), not a temperature")
}

fn mode_takes_temp(mode: AcMode) -> ControlError {
    ControlError::invalid(format!(
        "{mode} mode takes a target temperature, not a comfort offset"
    ))
}

/// The `Set` command for `change`, with mode, fan and setpoint the change
/// leaves out taken from `state`. Switching into Auto without an offset
/// uses offset 0.
///
/// # Errors
/// [`ControlError::Invalid`] if a needed value is neither in `change` nor
/// known in `state`, or the setpoint does not fit the resulting mode.
pub fn plan_set(change: &AcChange, state: &AcState) -> Result<AcCommand> {
    let (mode, fan, setpoint) = resolve_set(*change, state)?;
    Ok(AcCommand::Set {
        mode,
        fan,
        setpoint,
    })
}

/// The commands for `change`, in sending order: power off alone, or power
/// on with mode, fan and setpoint, or a `Set`; then the light. Values the
/// change leaves out come from `state` (see [`plan_set`]). Mode, fan or
/// setpoint without power is a `Set` whatever the power state: it never
/// turns the unit on.
///
/// # Errors
/// [`ControlError::Invalid`] as for [`AcChange::check`] and [`plan_set`].
pub fn plan_commands(change: &AcChange, state: &AcState) -> Result<Vec<AcCommand>> {
    change.check()?;
    let mut cmds = Vec::new();
    match change.power {
        Some(false) => cmds.push(AcCommand::PowerOff),
        Some(true) => {
            let (mode, fan, setpoint) = resolve_set(*change, state)?;
            cmds.push(AcCommand::PowerOnWith {
                mode,
                fan,
                setpoint,
            });
        }
        None if change.touches_set() => cmds.push(plan_set(change, state)?),
        None => {}
    }
    if let Some(on) = change.light {
        cmds.push(AcCommand::Light(on));
    }
    Ok(cmds)
}

/// Mode, fan and setpoint for `change`, filled from `state`.
fn resolve_set(change: AcChange, state: &AcState) -> Result<(AcMode, FanSpeed, Setpoint)> {
    let unknown = |what: &str, flag: &str| {
        ControlError::invalid(format!("the current {what} is not known yet; pass {flag}"))
    };
    let mode = change
        .mode
        .or(state.mode)
        .ok_or_else(|| unknown("mode", "--mode"))?;
    let fan = change
        .fan
        .or(state.fan)
        .ok_or_else(|| unknown("fan speed", "--fan"))?;
    let setpoint = if mode == AcMode::Auto {
        if change.temp.is_some() {
            return Err(auto_takes_offset());
        }
        let offset = match change.offset.or_else(|| state.comfort_offset()) {
            Some(offset) => offset,
            None => ComfortOffset::new(0)?,
        };
        Setpoint::Offset(offset)
    } else {
        if change.offset.is_some() {
            return Err(mode_takes_temp(mode));
        }
        let temp = change
            .temp
            .or_else(|| state.target_temp())
            .ok_or_else(|| unknown("target temperature", "--temp"))?;
        Setpoint::Temp(temp)
    };
    Ok((mode, fan, setpoint))
}

/// Whether `state` shows `cmd` applied (a report can stand in for a lost
/// ACK). A query is never confirmed this way.
fn confirms(cmd: &AcCommand, state: &AcState) -> bool {
    let shows_set = |mode: AcMode, fan: FanSpeed, setpoint: Setpoint| {
        state.mode == Some(mode)
            && state.fan == Some(fan)
            && match setpoint {
                Setpoint::Temp(t) => state.target_temp() == Some(t),
                Setpoint::Offset(o) => state.comfort_offset() == Some(o),
            }
    };
    match *cmd {
        AcCommand::PowerOff => state.power == Some(false),
        AcCommand::PowerOn => state.power == Some(true),
        AcCommand::PowerOnWith {
            mode,
            fan,
            setpoint,
        } => state.power == Some(true) && shows_set(mode, fan, setpoint),
        AcCommand::Set {
            mode,
            fan,
            setpoint,
        } => shows_set(mode, fan, setpoint),
        AcCommand::Light(on) => state.light == Some(on),
        AcCommand::Query(_) => false,
    }
}

#[derive(Debug, Default)]
struct Snapshot {
    state: AcState,
    connected: bool,
    provisioned: bool,
    /// When the current MQTT session started.
    connected_at: Option<Instant>,
    last_report: Option<Instant>,
    /// When each frame sent during this connection whose ACK has not
    /// arrived yet was sent, oldest first.
    unacked: VecDeque<Instant>,
}

impl Snapshot {
    /// Connected, and no report has arrived during this connection (a
    /// report from before a reconnect may be stale).
    fn needs_report(&self) -> bool {
        self.connected
            && match (self.last_report, self.connected_at) {
                (None, _) => true,
                (Some(report), Some(connected)) => report < connected,
                (Some(_), None) => false,
            }
    }
}

/// State shared by the controller and its event task.
#[derive(Debug)]
struct Shared {
    snapshot: Mutex<Snapshot>,
    /// Sequence numbers of ACK frames from the unit (always 0 on the real
    /// module, so only their arrival counts).
    acks: broadcast::Sender<u8>,
    /// Counts state reports; waiters watch it change.
    reports: watch::Sender<u64>,
    /// Counts the reports holding the full state group ([`FULL_STATE`]),
    /// i.e. the ones that answer a state query.
    full_reports: watch::Sender<u64>,
    milestones: broadcast::Sender<Milestone>,
    /// The next frame sequence number (wraps).
    seq: AtomicU8,
}

impl Shared {
    fn new() -> Self {
        Self {
            snapshot: Mutex::new(Snapshot::default()),
            acks: broadcast::Sender::new(ACK_QUEUE),
            reports: watch::Sender::new(0),
            full_reports: watch::Sender::new(0),
            milestones: broadcast::Sender::new(MILESTONE_QUEUE),
            seq: AtomicU8::new(1),
        }
    }

    /// Starts watching for a command's confirmation; call it before sending
    /// so that only ACKs and reports that arrive afterwards count. The ACKs
    /// still owed for earlier frames (sent within [`ACK_OWED_FOR`]) will be
    /// skipped; counting and subscribing under the snapshot lock, like
    /// [`on_packet`](Self::on_packet), keeps the two consistent.
    fn arm(&self) -> Pending<'_> {
        let mut reports = self.reports.subscribe();
        reports.borrow_and_update();
        let mut snap = self.snapshot();
        let now = Instant::now();
        while snap
            .unacked
            .front()
            .is_some_and(|&sent| now.duration_since(sent) > ACK_OWED_FOR)
        {
            snap.unacked.pop_front();
        }
        let owed = snap.unacked.len();
        let acks = self.acks.subscribe();
        drop(snap);
        Pending {
            shared: self,
            acks,
            reports,
            owed,
        }
    }

    /// Records a frame about to be sent: the unit owes one more ACK.
    fn expect_ack(&self) {
        self.snapshot().unacked.push_back(Instant::now());
    }

    /// Takes back [`expect_ack`](Self::expect_ack) for a frame whose send failed.
    fn unexpect_ack(&self) {
        self.snapshot().unacked.pop_back();
    }

    fn snapshot(&self) -> std::sync::MutexGuard<'_, Snapshot> {
        self.snapshot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, milestone: Milestone) {
        // No subscriber is fine: nobody is printing progress.
        let _ = self.milestones.send(milestone);
    }

    fn status(&self) -> AcStatus {
        let snap = self.snapshot();
        AcStatus {
            state: snap.state.clone(),
            connected: snap.connected,
            provisioned: snap.provisioned,
            last_report_age: snap.last_report.map(|t| t.elapsed()),
        }
    }

    /// A new MQTT session: not provisioned yet, and it owes nothing for
    /// frames sent to the old one.
    fn on_connected(&self) {
        let mut snap = self.snapshot();
        snap.connected = true;
        snap.provisioned = false;
        snap.connected_at = Some(Instant::now());
        snap.unacked.clear();
        drop(snap);
        self.emit(Milestone::MqttConnected);
    }

    /// The module acked our provisioning reply: it may be sent commands
    /// now, and it owes the ACK of the state query sent next. Both under one
    /// lock, so that a change armed as soon as it sees `provisioned` skips
    /// that ACK (see [`arm`](Self::arm)); the caller then sends the query
    /// with [`send_owed`].
    fn on_provisioned(&self, kind: ProvisioningKind) {
        let mut snap = self.snapshot();
        snap.provisioned = true;
        snap.unacked.push_back(Instant::now());
        drop(snap);
        self.emit(Milestone::Provisioned(kind));
    }

    fn on_disconnected(&self) {
        let mut snap = self.snapshot();
        snap.connected = false;
        snap.provisioned = false;
        snap.unacked.clear();
        drop(snap);
        self.emit(Milestone::MqttDisconnected);
    }

    /// Applies one `device_packet` (hex is never logged).
    fn on_packet(&self, hex: &str) {
        match tlv::decode_device_packet(hex) {
            Ok(DevicePacket::Data { tlvs, .. }) => {
                let changed = {
                    let mut snap = self.snapshot();
                    let before = snap.state.clone();
                    snap.state.apply(&tlvs);
                    snap.last_report = Some(Instant::now());
                    (snap.state != before).then(|| snap.state.clone())
                };
                let full = FULL_STATE
                    .iter()
                    .all(|&kind| tlvs.iter().any(|t| t.kind == kind));
                self.reports.send_modify(|n| *n = n.wrapping_add(1));
                if full {
                    self.full_reports.send_modify(|n| *n = n.wrapping_add(1));
                }
                tracing::debug!(fields = tlvs.len(), full, "ac: state report");
                if let Some(state) = changed {
                    self.emit(Milestone::State(state));
                }
            }
            Ok(DevicePacket::Ack { seq }) => {
                tracing::debug!(seq, "ac: ACK");
                // The oldest owed ACK is paid; see `arm`.
                let mut snap = self.snapshot();
                snap.unacked.pop_front();
                let _ = self.acks.send(seq);
                drop(snap);
            }
            Ok(DevicePacket::Unsupported { header, len }) => {
                tracing::debug!(b5 = header.b5, b6 = header.b6, len, "ac: unsupported frame");
            }
            Ok(DevicePacket::Modem { prefix, len }) => {
                tracing::debug!(prefix = %hex::encode(prefix), len, "ac: modem message");
            }
            Err(e) => tracing::warn!(error = %e, "ac: undecodable device packet dropped"),
        }
    }
}

/// A command's confirmation being waited for (see [`Shared::arm`]).
struct Pending<'a> {
    shared: &'a Shared,
    acks: broadcast::Receiver<u8>,
    reports: watch::Receiver<u64>,
    /// ACKs still owed for frames sent before this one; skipped.
    owed: usize,
}

impl Pending<'_> {
    /// `true` once the ACK after the [`owed`](Self::owed) ones arrives
    /// (whatever its seq: the module always sends 0) or a report leaves the
    /// state showing `cmd` applied; `false` if neither happens within
    /// `timeout`.
    async fn confirmed(mut self, cmd: &AcCommand, timeout: Duration) -> Result<bool> {
        let stopped = || ControlError::network("the controller stopped");
        let mut owed = self.owed;
        let wait = async {
            loop {
                tokio::select! {
                    ack = self.acks.recv() => match ack {
                        Ok(seq) if owed > 0 => {
                            owed -= 1;
                            tracing::debug!(seq, "ac: ACK of an earlier frame skipped");
                        }
                        Ok(seq) => {
                            tracing::debug!(seq, "ac: command acknowledged");
                            return Ok(());
                        }
                        // ACKs arrived after arming but were dropped: ours
                        // is among them if more were dropped than were owed.
                        Err(RecvError::Lagged(n)) => {
                            let n = usize::try_from(n).unwrap_or(usize::MAX);
                            if n > owed {
                                return Ok(());
                            }
                            owed -= n;
                        }
                        Err(RecvError::Closed) => return Err(stopped()),
                    },
                    changed = self.reports.changed() => {
                        changed.map_err(|_| stopped())?;
                        if confirms(cmd, &self.shared.snapshot().state) {
                            tracing::debug!("ac: command confirmed by a state report");
                            return Ok(());
                        }
                    }
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result.map(|()| true),
            Err(_) => Ok(false),
        }
    }
}

/// The running local cloud, broker and state for one module. Stop it with
/// [`shutdown`](Self::shutdown); dropping it also stops everything.
#[derive(Debug)]
pub struct AcController {
    shared: Arc<Shared>,
    broker: Arc<MqttBroker>,
    cloud: CloudServer,
    tasks: Tasks,
    /// One status refresh or change at a time.
    commands: tokio::sync::Mutex<()>,
    timeouts: Timeouts,
    https_addr: SocketAddr,
    mqtt_addr: SocketAddr,
}

impl AcController {
    /// Starts the HTTPS server and the broker for the module `uuid` (from
    /// `getDeviceInfo`), both for the controller's whole life.
    ///
    /// The cloud signs one CSR per controller (one provisioning session): a
    /// module that needs a new client certificate needs a new controller.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] for a bad config or uuid;
    /// [`ControlError::Network`] if a port cannot be bound.
    pub async fn start(config: &ServeConfig, pki: &Pki, uuid: &str) -> Result<Self> {
        Self::start_with(config, pki, uuid, Timeouts::PRODUCTION).await
    }

    pub(crate) async fn start_with(
        config: &ServeConfig,
        pki: &Pki,
        uuid: &str,
        timeouts: Timeouts,
    ) -> Result<Self> {
        let cloud_config = CloudConfig {
            bind: config.bind,
            port: config.https_port,
            advertised_host: config.host.clone(),
        };
        let mqtt_config = MqttConfig {
            bind: config.bind,
            port: config.mqtt_port,
            pinning: config.pinning.clone(),
        };
        let cloud = CloudSession::start(&cloud_config, pki, uuid).await?;
        let broker = Arc::new(MqttSession::start(&mqtt_config, pki, uuid).await?);
        let shared = Arc::new(Shared::new());
        let tasks = Tasks(vec![
            tokio::spawn(cloud_progress(cloud.progress(), Arc::clone(&shared))),
            tokio::spawn(event_loop(Arc::clone(&broker), Arc::clone(&shared))),
        ]);
        Ok(Self {
            https_addr: cloud.local_addr(),
            mqtt_addr: broker.local_addr(),
            shared,
            broker,
            cloud,
            tasks,
            commands: tokio::sync::Mutex::new(()),
            timeouts,
        })
    }

    /// The HTTPS address actually bound.
    #[must_use]
    pub fn https_addr(&self) -> SocketAddr {
        self.https_addr
    }

    /// The MQTTS address actually bound.
    #[must_use]
    pub fn mqtt_addr(&self) -> SocketAddr {
        self.mqtt_addr
    }

    /// Progress events from now on (see [`Milestone`]).
    #[must_use]
    pub fn milestones(&self) -> broadcast::Receiver<Milestone> {
        self.shared.milestones.subscribe()
    }

    /// The current state. If the module is connected but has not reported
    /// since it (re)connected, asks it first and waits up to 10 s for the
    /// report; if none comes, the result keeps the older state and its age.
    ///
    /// # Errors
    /// None today; `Result` leaves room for a stopped controller.
    pub async fn status(&self) -> Result<AcStatus> {
        if !self.shared.snapshot().needs_report() {
            return Ok(self.shared.status());
        }
        let _one_at_a_time = self.commands.lock().await;
        if self.shared.snapshot().needs_report() {
            match self.refresh().await {
                Ok(true) => {}
                Ok(false) => tracing::warn!("ac: no state report after a query"),
                Err(e) => tracing::warn!(error = %e, "ac: state query failed"),
            }
        }
        Ok(self.shared.status())
    }

    /// Applies `change` and returns the state the unit reports afterwards.
    ///
    /// Sends the commands of [`plan_commands`] (power off alone, or power
    /// on with mode, fan and setpoint, or one `Set`; missing values from the
    /// current state, which is queried first if needed), then the light;
    /// each must be confirmed within 5 s by an ACK or a state report showing
    /// it. Then queries the state and waits up to 10 s for the full report
    /// that answers it (a command's trailing partial report does not).
    ///
    /// # Errors
    /// [`ControlError::Invalid`] for an impossible change (see
    /// [`AcChange::check`], [`plan_set`]); [`ControlError::Network`] if the
    /// module is not connected, a send fails, a command is not confirmed or
    /// the read-back report does not arrive in time.
    pub async fn apply(&self, change: AcChange) -> Result<AcStatus> {
        change.check()?;
        let _one_at_a_time = self.commands.lock().await;
        let status = self.shared.status();
        if !status.connected {
            return Err(ControlError::network("the AC is not connected to this Mac"));
        }
        if !status.provisioned {
            return Err(ControlError::network(
                "the AC is connected but has not finished provisioning; try again in a few seconds",
            ));
        }
        for cmd in self.plan(&change).await? {
            self.command(&cmd).await?;
        }
        if !self.refresh().await? {
            return Err(ControlError::network(format!(
                "the AC acknowledged the change but sent no state report within {} s; \
                 check with `controlz ac status`",
                self.timeouts.report.as_secs()
            )));
        }
        Ok(self.shared.status())
    }

    /// [`plan_commands`] on the current state, querying the unit once if
    /// the state lacks what the change needs.
    async fn plan(&self, change: &AcChange) -> Result<Vec<AcCommand>> {
        let state = self.shared.status().state;
        if let Ok(cmds) = plan_commands(change, &state) {
            return Ok(cmds);
        }
        self.refresh().await?;
        plan_commands(change, &self.shared.status().state)
    }

    /// Sends `cmd` with the next sequence number; returns that number.
    async fn send(&self, cmd: &AcCommand) -> Result<u8> {
        send(&self.broker, &self.shared, cmd).await
    }

    /// Sends `cmd` and waits for its confirmation. The caller holds the
    /// command lock, so no other command is in flight; arming before the
    /// send leaves out any ACK that came earlier and skips the ACKs still
    /// owed for earlier frames (a late one must not confirm this command).
    async fn command(&self, cmd: &AcCommand) -> Result<()> {
        let pending = self.shared.arm();
        let seq = self.send(cmd).await?;
        if pending.confirmed(cmd, self.timeouts.ack).await? {
            Ok(())
        } else {
            Err(ControlError::network(format!(
                "no ACK from the AC within {} s, and no state report showing the change \
                 (frame {seq})",
                self.timeouts.ack.as_secs()
            )))
        }
    }

    /// Queries the state; `true` once a full report ([`FULL_STATE`]) arrived
    /// in time. Partial reports (a command's, which comes after its ACK and
    /// so may still be on its way; or a spontaneous one) do not count.
    async fn refresh(&self) -> Result<bool> {
        let mut full = self.shared.full_reports.subscribe();
        full.borrow_and_update();
        self.send(&AcCommand::Query(QueryGroup::State)).await?;
        let changed = tokio::time::timeout(self.timeouts.report, full.changed()).await;
        Ok(matches!(changed, Ok(Ok(()))))
    }

    /// Stops the event tasks, the HTTPS server and the broker, and waits
    /// until their listeners are closed.
    pub async fn shutdown(self) {
        let Self {
            mut tasks,
            cloud,
            broker,
            ..
        } = self;
        tasks.stop().await;
        cloud.shutdown().await;
        // The tasks held the only other references, so this succeeds; if it
        // ever did not, dropping the last reference still stops the broker.
        if let Ok(broker) = Arc::try_unwrap(broker) {
            broker.shutdown().await;
        }
    }
}

/// The controller's background tasks, aborted on drop (they hold the
/// broker, which would otherwise outlive the controller).
#[derive(Debug)]
struct Tasks(Vec<JoinHandle<()>>);

impl Tasks {
    async fn stop(&mut self) {
        for task in self.0.drain(..) {
            task.abort();
            // A cancelled task is the expected outcome.
            let _ = task.await;
        }
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Sends `cmd` with the next sequence number, recording its ACK as owed
/// first (so that the ACK always finds the entry); returns that number.
async fn send(broker: &MqttBroker, shared: &Shared, cmd: &AcCommand) -> Result<u8> {
    shared.expect_ack();
    send_owed(broker, shared, cmd).await
}

/// [`send`] for a frame whose ACK the caller already recorded as owed;
/// takes that record back only if the frame surely never left.
///
/// A broker error keeps the record: "sending timed out" comes after the
/// frame was queued, so the unit may still get and ACK it, and dropping
/// the record would let that ACK confirm a later command. The other broker
/// errors (not connected, disconnected, busy) can't be told apart from it
/// here; for the first two a reconnect clears the records anyway, and a
/// spare record for "busy" makes at most a later command skip its own ACK
/// (it can still be confirmed by its report) until it ages out after
/// [`ACK_OWED_FOR`]. Missing an ACK is safe; taking a wrong one is not.
async fn send_owed(broker: &MqttBroker, shared: &Shared, cmd: &AcCommand) -> Result<u8> {
    let seq = shared.seq.fetch_add(1, Ordering::Relaxed);
    let hex = match tlv::encode(cmd, seq) {
        Ok(hex) => hex,
        Err(e) => {
            shared.unexpect_ack();
            return Err(e);
        }
    };
    if let Err(e) = broker.send_packet(&hex).await {
        // `Invalid` is our own hex rejected before anything was queued.
        if matches!(e, ControlError::Invalid { .. }) {
            shared.unexpect_ack();
        }
        return Err(e);
    }
    tracing::debug!(seq, "ac: command sent");
    Ok(seq)
}

/// Turns the cloud's request counters into milestones.
async fn cloud_progress(mut progress: watch::Receiver<CloudProgress>, shared: Arc<Shared>) {
    let mut last = CloudProgress::default();
    while progress.changed().await.is_ok() {
        let now = *progress.borrow_and_update();
        if now.route > last.route {
            shared.emit(Milestone::RouteServed);
        }
        if now.ca > last.ca {
            shared.emit(Milestone::CaServed);
        }
        if now.client_cert_issued && !last.client_cert_issued {
            shared.emit(Milestone::ClientCertIssued);
        }
        last = now;
    }
}

/// Folds broker events into the shared state until the broker goes away.
async fn event_loop(broker: Arc<MqttBroker>, shared: Arc<Shared>) {
    while let Some(event) = broker.next_event().await {
        match event {
            DeviceEvent::Connected => shared.on_connected(),
            DeviceEvent::Provisioned { kind } => {
                shared.on_provisioned(kind);
                // The module is subscribed now (it acked our reply), so
                // this is the first moment a query can reach it.
                let query = AcCommand::Query(QueryGroup::State);
                if let Err(e) = send_owed(&broker, &shared, &query).await {
                    tracing::warn!(error = %e, "ac: state query after provisioning failed");
                }
            }
            DeviceEvent::DevicePacket { hex } => shared.on_packet(&hex),
            DeviceEvent::Disconnected => shared.on_disconnected(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ac::mqtt::test_client::Client;
    use crate::ac::pki::Identity;
    use crate::ac::pki::test_fixture::{FIXTURE, csr_pem, p256_key};
    use crate::ac::tlv::{Direction, Frame, Header, Tlv, field};
    use crate::secret::Secret;
    use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};
    use openssl::x509::X509;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::net::Ipv4Addr;
    use std::pin::Pin;
    use std::sync::LazyLock;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tokio_openssl::SslStream;

    const UUID: &str = "00000000-fake-0000-0000-000000000000";
    const LOOPBACK: &str = "127.0.0.1";
    const WAIT: Duration = Duration::from_secs(5);
    const B6_REPORT: u8 = 0x04;
    const B6_ACK: u8 = 0x10;
    const B6_COMMAND: u8 = 0x01;
    const B6_QUERY: u8 = 0x02;

    /// The fixture CAs with a server certificate for the loopback IP (IP SAN).
    static PKI: LazyLock<Pki> = LazyLock::new(|| Pki {
        server: FIXTURE.ca.issue_server(LOOPBACK).unwrap(),
        ..FIXTURE.clone()
    });

    fn config() -> ServeConfig {
        ServeConfig {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            host: LOOPBACK.to_owned(),
            https_port: 0,
            mqtt_port: 0,
            pinning: None,
        }
    }

    async fn controller(timeouts: Timeouts) -> AcController {
        AcController::start_with(&config(), &PKI, UUID, timeouts)
            .await
            .unwrap()
    }

    fn device_topic() -> String {
        format!("lime/devices/{UUID}")
    }

    fn prov_topic() -> String {
        format!("clip/provisioning/devices/{UUID}")
    }

    fn msg_topic() -> String {
        format!("clip/devices/{UUID}/message")
    }

    fn message(cmd: &str, data: &Value) -> Vec<u8> {
        json!({"did": UUID, "mid": 1, "cmd": cmd, "type": 0, "data": data})
            .to_string()
            .into_bytes()
    }

    /// A TLS 1.2 client like the module. With `trust`, it verifies our CA
    /// and the loopback IP SAN; `identity` is its client certificate.
    fn connector(trust: Option<&X509>, identity: Option<&Identity>) -> SslConnector {
        let mut b = SslConnector::builder(SslMethod::tls_client()).unwrap();
        b.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        b.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
        b.set_cipher_list("ECDHE-RSA-AES256-SHA@SECLEVEL=0")
            .unwrap();
        match trust {
            Some(ca) => b.cert_store_mut().add_cert(ca.clone()).unwrap(),
            None => b.set_verify(SslVerifyMode::NONE),
        }
        if let Some(id) = identity {
            b.set_certificate(id.cert()).unwrap();
            b.set_private_key(id.key()).unwrap();
        }
        b.build()
    }

    async fn tls(
        addr: SocketAddr,
        trust: Option<&X509>,
        identity: Option<&Identity>,
    ) -> SslStream<TcpStream> {
        let ssl = connector(trust, identity)
            .configure()
            .unwrap()
            .into_ssl(LOOPBACK)
            .unwrap();
        let tcp = TcpStream::connect(addr).await.unwrap();
        let mut stream = SslStream::new(ssl, tcp).unwrap();
        Pin::new(&mut stream)
            .connect()
            .await
            .expect("TLS handshake");
        stream
    }

    /// One HTTPS request (`Connection: close`); returns (status, JSON body).
    async fn https(
        addr: SocketAddr,
        trust: Option<&X509>,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> (u16, Value) {
        let mut stream = tls(addr, trust, None).await;
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {LOOPBACK}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        let mut raw = Vec::new();
        let _ = tokio::time::timeout(WAIT, stream.read_to_end(&mut raw)).await;
        let text = String::from_utf8(raw).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").expect("a full response");
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap())
    }

    /// A frame from the unit: `b5` = 2 for reports, 1 for ACKs (as captured).
    fn uart_hex(b5: u8, b6: u8, seq: u8, tlvs: &[Tlv]) -> String {
        let frame = Frame {
            header: Header {
                direction: Direction::FromAppliance,
                b5,
                b6,
                seq,
            },
            payload: tlv::encode_tlvs(tlvs).unwrap(),
        };
        let mut bytes = tlv::UART_PREFIX.to_vec();
        bytes.extend(frame.to_bytes().unwrap());
        hex::encode(bytes)
    }

    /// The real module's ACK: `87 01 10`, always seq 0 (T1.9 device run).
    fn ack_hex() -> String {
        uart_hex(0x01, B6_ACK, 0, &[])
    }

    fn report_hex(tlvs: &[Tlv]) -> String {
        uart_hex(0x02, B6_REPORT, 0x56, tlvs)
    }

    /// What the mock AC received: (b6, TLVs).
    type Seen = mpsc::UnboundedReceiver<(u8, Vec<Tlv>)>;

    /// How the mock AC answers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Unit {
        /// ACKs (seq 0) every frame, then reports: the full state after a
        /// query, the applied fields after a command (those
        /// [`COMMAND_REPORT_DELAY`] after the ACK, as on the real unit).
        Responsive,
        /// Reports as `Responsive`, but its ACKs never arrive.
        LosesAcks,
        /// Never answers.
        Silent,
        /// The real unit's order (T1.9 device run): a command's ACK comes
        /// [`COMMAND_REPORT_DELAY`] before its report, a query's ACK [`QUERY_ACK_DELAY`] after its
        /// report (so after whatever the controller sends next). With
        /// `loses_commands`, commands get neither ACK nor report; queries
        /// still do.
        RealTiming { loses_commands: bool },
    }

    /// How long after its report [`Unit::RealTiming`] ACKs a query (about
    /// a second on the real unit; shorter keeps the tests fast).
    const QUERY_ACK_DELAY: Duration = Duration::from_millis(500);

    /// How long after a command's ACK the unit sends its partial report
    /// (about 0.1–0.2 s on the real unit). Frames sent meanwhile wait.
    const COMMAND_REPORT_DELAY: Duration = Duration::from_millis(200);

    /// How long after reading a query the unit sends its full report
    /// (about 0.5 s on the real unit). Frames sent meanwhile wait.
    const QUERY_REPORT_DELAY: Duration = Duration::from_millis(300);

    /// A simulated unit like the real one: power on sent alone is acknowledged
    /// but ignored; other commands change its state and are reported back.
    /// While it is on, each query finds the room half a degree cooler, so
    /// only the full report shows the room temperature a test expects.
    fn mock_ac(mut device: Client<SslStream<TcpStream>>, unit: Unit) -> (JoinHandle<()>, Seen) {
        let (seen_tx, seen) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut state = BTreeMap::from([
                (field::POWER, 0),
                (field::MODE, 0),       // cool
                (field::FAN, 8),        // auto
                (field::ROOM_TEMP, 52), // 26.0 °C
                (field::SETPOINT, 48),  // 24.0 °C
            ]);
            loop {
                let (topic, body) = device.recv_json().await;
                assert_eq!(topic, device_topic());
                if body["cmd"] != "packet" {
                    continue;
                }
                let (prefix, frame) =
                    tlv::decode_command_packet(body["data"].as_str().unwrap()).unwrap();
                assert_eq!(prefix, tlv::COMMAND_PREFIX);
                let tlvs = tlv::decode_tlvs(&frame.payload).unwrap();
                let _ = seen_tx.send((frame.header.b6, tlvs.clone()));
                if unit == Unit::Silent {
                    continue;
                }
                let query = frame.header.b6 == B6_QUERY;
                let lost = unit
                    == Unit::RealTiming {
                        loses_commands: true,
                    };
                if lost && !query {
                    continue;
                }
                let ack_first = match unit {
                    Unit::Responsive => true,
                    Unit::RealTiming { .. } => !query,
                    Unit::LosesAcks | Unit::Silent => false,
                };
                if ack_first {
                    device
                        .publish(&msg_topic(), &message("device_packet", &json!(ack_hex())))
                        .await;
                }
                let report: Vec<Tlv> = if query {
                    tokio::time::sleep(QUERY_REPORT_DELAY).await;
                    if state[&field::POWER] == 1 {
                        *state.entry(field::ROOM_TEMP).or_default() -= 1;
                    }
                    state
                        .iter()
                        .map(|(&kind, &value)| Tlv { kind, value })
                        .collect()
                } else if tlvs == [tlv(field::POWER, 1)] {
                    continue;
                } else {
                    for t in &tlvs {
                        state.insert(t.kind, t.value);
                    }
                    if ack_first {
                        tokio::time::sleep(COMMAND_REPORT_DELAY).await;
                    }
                    tlvs
                };
                device
                    .publish(
                        &msg_topic(),
                        &message("device_packet", &json!(report_hex(&report))),
                    )
                    .await;
                if query && matches!(unit, Unit::RealTiming { .. }) {
                    // Frames sent meanwhile wait in the socket, as on the
                    // real unit, which ACKs them after this one.
                    tokio::time::sleep(QUERY_ACK_DELAY).await;
                    device
                        .publish(&msg_topic(), &message("device_packet", &json!(ack_hex())))
                        .await;
                }
            }
        });
        (task, seen)
    }

    /// MQTT CONNECT with `identity`, `preDeploy` *before* SUBSCRIBE (T1.5
    /// check a), SUBSCRIBE, then the ack of the `completeProvisioning` that
    /// follows the SUBACK.
    async fn provision(addr: SocketAddr, identity: &Identity) -> Client<SslStream<TcpStream>> {
        let stream = tls(addr, Some(FIXTURE.ca.cert()), Some(identity)).await;
        let mut device = Client { stream };
        assert_eq!(device.connect(UUID).await, 0);
        device
            .publish(&prov_topic(), &message("preDeploy", &json!({})))
            .await;
        assert_eq!(device.subscribe(&device_topic(), 1).await, 1);
        let (topic, reply) = device.recv_json().await;
        assert_eq!(topic, device_topic());
        assert_eq!(reply["cmd"], "completeProvisioning");
        device
            .publish(
                &prov_topic(),
                &message("completeProvisioning_ack", &json!({"result": 0})),
            )
            .await;
        device
    }

    /// A client certificate straight from the client CA (skips HTTPS).
    fn client_identity() -> Identity {
        let key = p256_key();
        let pem = FIXTURE
            .client_ca
            .sign_csr(&csr_pem(&key, &[UUID]), UUID, 30)
            .unwrap();
        let key_pem =
            Secret::new(String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap());
        Identity::from_pem(&pem, &key_pem).unwrap()
    }

    async fn milestone(rx: &mut broadcast::Receiver<Milestone>) -> Milestone {
        tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("a milestone")
            .expect("milestones open")
    }

    /// A connected, provisioned mock AC whose first report has arrived.
    async fn ready(ctrl: &AcController, unit: Unit) -> (JoinHandle<()>, Seen) {
        let mut milestones = ctrl.milestones();
        let device = provision(ctrl.mqtt_addr(), &client_identity()).await;
        let (task, mut seen) = mock_ac(device, unit);
        loop {
            if let Milestone::Provisioned(_) = milestone(&mut milestones).await {
                break;
            }
        }
        // The automatic query after provisioning ...
        let (b6, tlvs) = tokio::time::timeout(WAIT, seen.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(b6, B6_QUERY);
        assert_eq!(
            tlvs,
            vec![Tlv {
                kind: field::QUERY,
                value: 2
            }]
        );
        // ... and, if the unit answers, its report.
        if unit != Unit::Silent {
            loop {
                if let Milestone::State(_) = milestone(&mut milestones).await {
                    break;
                }
            }
        }
        (task, seen)
    }

    fn drain(seen: &mut Seen) -> Vec<(u8, Vec<Tlv>)> {
        std::iter::from_fn(|| seen.try_recv().ok()).collect()
    }

    fn tlv(kind: u16, value: u32) -> Tlv {
        Tlv { kind, value }
    }

    // ---- Acceptance: full loopback provisioning ----

    #[tokio::test]
    async fn mock_device_provisions_and_reports_state() {
        let ctrl = controller(Timeouts::PRODUCTION).await;
        let mut milestones = ctrl.milestones();
        let https_addr = ctrl.https_addr();

        // 1. /route, accepting any certificate, advertises the bare IP.
        let (status, body) = https(https_addr, None, "GET", "/route", b"").await;
        assert_eq!(status, 200);
        assert_eq!(
            body["result"],
            json!({"apiServer": "https://127.0.0.1:443", "mqttServer": "ssl://127.0.0.1:8883"})
        );
        assert_eq!(milestone(&mut milestones).await, Milestone::RouteServed);

        // 2. Our CA.
        let (status, body) = https(
            https_addr,
            None,
            "GET",
            "/route/certificate?name=aws-iot",
            b"",
        )
        .await;
        assert_eq!(status, 200);
        let ca = X509::from_pem(
            body["result"]["certificatePem"]
                .as_str()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(milestone(&mut milestones).await, Milestone::CaServed);

        // 3. The CSR, verifying our server by its IP SAN against that CA.
        let key = p256_key();
        let path = format!("/device/{UUID}/certificate");
        let csr = csr_pem(&key, &[UUID]);
        let (status, body) = https(https_addr, Some(&ca), "POST", &path, csr.as_bytes()).await;
        assert_eq!(status, 200);
        let cert_pem = body["result"]["certificatePem"].as_str().unwrap();
        assert_eq!(
            milestone(&mut milestones).await,
            Milestone::ClientCertIssued
        );
        let key_pem =
            Secret::new(String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap());
        let identity = Identity::from_pem(cert_pem, &key_pem).unwrap();

        // 4. MQTT with the issued certificate; preDeploy before SUBSCRIBE.
        let device = provision(ctrl.mqtt_addr(), &identity).await;
        assert_eq!(milestone(&mut milestones).await, Milestone::MqttConnected);
        assert_eq!(
            milestone(&mut milestones).await,
            Milestone::Provisioned(ProvisioningKind::PreDeploy)
        );

        // 5. The automatic query brings a report.
        let (task, _seen) = mock_ac(device, Unit::Responsive);
        let Milestone::State(state) = milestone(&mut milestones).await else {
            panic!("expected a state milestone");
        };
        assert_eq!(state.power, Some(false));
        let status = ctrl.status().await.unwrap();
        assert!(status.connected && status.provisioned);
        assert!(status.last_report_age.unwrap() < WAIT);
        assert_eq!(status.state.mode, Some(AcMode::Cool));
        assert_eq!(status.state.fan, Some(FanSpeed::Auto));
        assert_eq!(
            status.state.target_temp().map(TargetTemp::celsius),
            Some(24.0)
        );
        assert_eq!(
            status.state.room_temp.map(tlv::RoomTemp::celsius),
            Some(26.0)
        );

        task.abort();
        let mqtt_addr = ctrl.mqtt_addr();
        ctrl.shutdown().await;
        assert!(
            TcpStream::connect(https_addr).await.is_err(),
            "HTTPS closed"
        );
        assert!(TcpStream::connect(mqtt_addr).await.is_err(), "MQTT closed");
    }

    // ---- apply ----

    /// Power on goes in one frame with mode, fan and setpoint (here the
    /// change's temperature, the rest from the state); the ACKs carry seq 0.
    #[tokio::test]
    async fn apply_powers_on_with_state_then_reads_back() {
        let ctrl = controller(Timeouts::PRODUCTION).await;
        let (task, mut seen) = ready(&ctrl, Unit::Responsive).await;
        let before = ctrl.status().await.unwrap();
        assert_eq!(before.state.power, Some(false));
        drain(&mut seen);

        let change = AcChange {
            power: Some(true),
            temp: Some(TargetTemp::from_celsius(22.5).unwrap()),
            ..AcChange::default()
        };
        let after = ctrl.apply(change).await.unwrap();
        assert_eq!(after.state.power, Some(true));
        assert_eq!(
            after.state.target_temp().map(TargetTemp::celsius),
            Some(22.5)
        );
        assert_eq!(after.state.mode, Some(AcMode::Cool), "kept from the state");
        assert_eq!(after.state.fan, Some(FanSpeed::Auto), "kept from the state");
        assert_eq!(
            after.state.room_temp.map(tlv::RoomTemp::celsius),
            Some(25.5),
            "the query's full report, not the command's partial one"
        );
        assert_eq!(
            drain(&mut seen),
            vec![
                (
                    B6_COMMAND,
                    vec![
                        tlv(field::MODE, 0),
                        tlv(field::POWER, 1),
                        tlv(field::FAN, 8),
                        tlv(field::SETPOINT, 45)
                    ]
                ),
                (B6_QUERY, vec![tlv(field::QUERY, 2)]),
            ]
        );

        // Auto without an offset uses 0 (raw 34); then the light alone.
        let change = AcChange {
            mode: Some(AcMode::Auto),
            light: Some(false),
            ..AcChange::default()
        };
        let after = ctrl.apply(change).await.unwrap();
        assert_eq!(
            after.state.comfort_offset().map(ComfortOffset::get),
            Some(0)
        );
        assert_eq!(after.state.light, Some(false));
        assert_eq!(
            after.state.room_temp.map(tlv::RoomTemp::celsius),
            Some(25.0)
        );
        assert_eq!(
            drain(&mut seen),
            vec![
                (
                    B6_COMMAND,
                    vec![
                        tlv(field::MODE, 3),
                        tlv(field::FAN, 8),
                        tlv(field::SETPOINT, 34)
                    ]
                ),
                (B6_COMMAND, vec![tlv(field::LIGHT, 0)]),
                (B6_QUERY, vec![tlv(field::QUERY, 2)]),
            ]
        );
        task.abort();
        ctrl.shutdown().await;
    }

    #[tokio::test]
    async fn missing_ack_or_report_times_out() {
        let short = Timeouts {
            ack: Duration::from_millis(300),
            report: Duration::from_millis(300),
        };
        let ctrl = controller(short).await;
        let (task, mut seen) = ready(&ctrl, Unit::Silent).await;

        // No report: status asks once more, then gives up without an age.
        let status = ctrl.status().await.unwrap();
        assert!(status.connected);
        assert_eq!(status.last_report_age, None);
        assert_eq!(status.state, AcState::default());
        assert_eq!(
            drain(&mut seen),
            vec![(B6_QUERY, vec![tlv(field::QUERY, 2)])]
        );

        // No ACK and no report: the change fails after the power command,
        // nothing else sent.
        let err = ctrl
            .apply(AcChange {
                power: Some(false),
                light: Some(true),
                ..AcChange::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ControlError::Network { .. }), "{err:?}");
        assert!(err.to_string().contains("no ACK"), "{err}");
        assert_eq!(
            drain(&mut seen),
            vec![(B6_COMMAND, vec![tlv(field::POWER, 0)])]
        );
        task.abort();
        ctrl.shutdown().await;
    }

    /// A lost ACK does not fail a change the unit's report shows applied.
    #[tokio::test]
    async fn report_confirms_a_command_whose_ack_is_lost() {
        let short = Timeouts {
            ack: Duration::from_millis(1500),
            report: Duration::from_secs(5),
        };
        let ctrl = controller(short).await;
        let (task, mut seen) = ready(&ctrl, Unit::LosesAcks).await;
        drain(&mut seen);
        let after = ctrl
            .apply(AcChange {
                power: Some(true),
                fan: Some(FanSpeed::High),
                ..AcChange::default()
            })
            .await
            .unwrap();
        assert_eq!(after.state.power, Some(true));
        assert_eq!(after.state.fan, Some(FanSpeed::High));
        assert_eq!(
            drain(&mut seen),
            vec![
                (
                    B6_COMMAND,
                    vec![
                        tlv(field::MODE, 0),
                        tlv(field::POWER, 1),
                        tlv(field::FAN, FanSpeed::High.code().into()),
                        tlv(field::SETPOINT, 48)
                    ]
                ),
                (B6_QUERY, vec![tlv(field::QUERY, 2)]),
            ]
        );
        task.abort();
        ctrl.shutdown().await;
    }

    /// The real unit's timing: right after the report of the query that
    /// follows provisioning, that query's ACK is still owed. A change sent
    /// then skips it and is confirmed by its own ACK.
    #[tokio::test]
    async fn apply_skips_the_late_ack_of_the_query_after_provisioning() {
        let ctrl = controller(Timeouts::PRODUCTION).await;
        let unit = Unit::RealTiming {
            loses_commands: false,
        };
        let (task, mut seen) = ready(&ctrl, unit).await;
        drain(&mut seen);
        assert_eq!(ctrl.shared.snapshot().unacked.len(), 1, "query ACK owed");

        let after = ctrl
            .apply(AcChange {
                power: Some(false),
                light: Some(true),
                ..AcChange::default()
            })
            .await
            .unwrap();
        assert_eq!(after.state.power, Some(false));
        assert_eq!(after.state.light, Some(true));
        assert_eq!(
            drain(&mut seen),
            vec![
                (B6_COMMAND, vec![tlv(field::POWER, 0)]),
                (B6_COMMAND, vec![tlv(field::LIGHT, 1)]),
                (B6_QUERY, vec![tlv(field::QUERY, 2)]),
            ]
        );
        task.abort();
        ctrl.shutdown().await;
    }

    /// The same timing with a command the unit loses: the query's late ACK
    /// arrives while the command waits, but must not confirm it.
    #[tokio::test]
    async fn a_lost_command_is_not_confirmed_by_the_late_query_ack() {
        let short = Timeouts {
            ack: QUERY_ACK_DELAY * 4,
            report: Duration::from_secs(5),
        };
        let ctrl = controller(short).await;
        let unit = Unit::RealTiming {
            loses_commands: true,
        };
        let (task, mut seen) = ready(&ctrl, unit).await;
        drain(&mut seen);
        assert_eq!(ctrl.shared.snapshot().unacked.len(), 1, "query ACK owed");

        let err = ctrl
            .apply(AcChange {
                power: Some(false),
                light: Some(true),
                ..AcChange::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ControlError::Network { .. }), "{err:?}");
        assert!(err.to_string().contains("no ACK"), "{err}");
        assert_eq!(
            drain(&mut seen),
            vec![(B6_COMMAND, vec![tlv(field::POWER, 0)])]
        );
        // The query's ACK did arrive (it paid its debt); the lost
        // command's is still owed.
        assert_eq!(ctrl.shared.snapshot().unacked.len(), 1);
        task.abort();
        ctrl.shutdown().await;
    }

    /// The real unit's full state report (fake values), as `device_packet` hex.
    fn full_report_fixture_hex() -> String {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ac_tlv/state_report_full.json"
        )))
        .unwrap();
        fixture["data"].as_str().unwrap().to_owned()
    }

    /// A partial report that arrives after the query is sent (a command's
    /// trailing report, or a spontaneous one) is applied but does not
    /// answer the query; the unit's full report does.
    #[tokio::test]
    async fn only_a_full_report_answers_a_query() {
        let short = Timeouts {
            ack: Duration::from_millis(300),
            report: Duration::from_millis(400),
        };
        let ctrl = controller(short).await;
        let (task, mut seen) = ready(&ctrl, Unit::Silent).await;
        drain(&mut seen);

        let partial = later(
            &ctrl.shared,
            report_hex(&[tlv(field::POWER, 1), tlv(field::SETPOINT, 45)]),
        );
        assert!(!ctrl.refresh().await.unwrap(), "a partial report");
        partial.await.unwrap();
        let state = ctrl.shared.status().state;
        assert_eq!(state.power, Some(true), "still applied");
        assert_eq!(state.room_temp, None);

        let full = later(&ctrl.shared, full_report_fixture_hex());
        assert!(ctrl.refresh().await.unwrap(), "the full report");
        full.await.unwrap();
        let state = ctrl.shared.status().state;
        assert_eq!(state.fan, Some(FanSpeed::Mid));
        assert_eq!(
            state.room_temp.map(tlv::RoomTemp::celsius),
            Some(24.5),
            "from the fixture"
        );
        assert_eq!(
            drain(&mut seen),
            vec![
                (B6_QUERY, vec![tlv(field::QUERY, 2)]),
                (B6_QUERY, vec![tlv(field::QUERY, 2)]),
            ]
        );
        task.abort();
        ctrl.shutdown().await;
    }

    #[tokio::test]
    async fn apply_needs_a_connected_device() {
        let ctrl = controller(Timeouts::PRODUCTION).await;
        let status = ctrl.status().await.unwrap();
        assert!(!status.connected && !status.provisioned);
        let err = ctrl
            .apply(AcChange {
                power: Some(false),
                ..AcChange::default()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
        ctrl.shutdown().await;
    }

    /// After a reconnect without re-provisioning, `status` does not trust
    /// the report from the old connection: it queries once more.
    #[tokio::test]
    async fn status_queries_again_after_a_reconnect() {
        let ctrl = controller(Timeouts::PRODUCTION).await;
        let (task, mut seen) = ready(&ctrl, Unit::Responsive).await;
        let mut milestones = ctrl.milestones();
        // A fresh report: status answers from memory, no query.
        ctrl.status().await.unwrap();
        assert_eq!(drain(&mut seen), vec![]);

        task.abort();
        let _ = task.await;
        while milestone(&mut milestones).await != Milestone::MqttDisconnected {}

        // Reconnect and subscribe, but send no provisioning request.
        let stream = tls(
            ctrl.mqtt_addr(),
            Some(FIXTURE.ca.cert()),
            Some(&client_identity()),
        )
        .await;
        let mut device = Client { stream };
        assert_eq!(device.connect(UUID).await, 0);
        assert_eq!(device.subscribe(&device_topic(), 1).await, 1);
        while milestone(&mut milestones).await != Milestone::MqttConnected {}
        let (task, mut seen) = mock_ac(device, Unit::Responsive);

        let status = ctrl.status().await.unwrap();
        assert!(status.connected && !status.provisioned);
        assert_eq!(
            drain(&mut seen),
            vec![(B6_QUERY, vec![tlv(field::QUERY, 2)])]
        );
        assert!(status.last_report_age.unwrap() < WAIT);
        // Fresh again: no second query.
        ctrl.status().await.unwrap();
        assert_eq!(drain(&mut seen), vec![]);
        task.abort();
        ctrl.shutdown().await;
    }

    /// After a reconnect, a change waits for the new session's provisioning:
    /// until then nothing is sent; once it is acked, changes go through.
    #[tokio::test]
    async fn apply_refuses_until_provisioned_after_a_reconnect() {
        let ctrl = controller(Timeouts::PRODUCTION).await;
        let (task, _seen) = ready(&ctrl, Unit::Responsive).await;
        let mut milestones = ctrl.milestones();
        task.abort();
        let _ = task.await;
        while milestone(&mut milestones).await != Milestone::MqttDisconnected {}

        // Reconnect and subscribe, but send no provisioning request yet.
        let stream = tls(
            ctrl.mqtt_addr(),
            Some(FIXTURE.ca.cert()),
            Some(&client_identity()),
        )
        .await;
        let mut device = Client { stream };
        assert_eq!(device.connect(UUID).await, 0);
        assert_eq!(device.subscribe(&device_topic(), 1).await, 1);
        while milestone(&mut milestones).await != Milestone::MqttConnected {}

        let off = AcChange {
            power: Some(false),
            ..AcChange::default()
        };
        let err = ctrl.apply(off).await.unwrap_err();
        assert!(matches!(err, ControlError::Network { .. }), "{err:?}");
        assert!(
            err.to_string().contains("has not finished provisioning"),
            "{err}"
        );

        // Now the deploy; the first thing the unit receives is our reply,
        // so the refused change sent nothing. The ack comes on the message
        // topic, as on the real AC.
        device
            .publish(&prov_topic(), &message("deploy", &json!({})))
            .await;
        let (topic, reply) = device.recv_json().await;
        assert_eq!(topic, device_topic());
        assert_eq!(reply["cmd"], "completeProvisioning");
        device
            .publish(
                &msg_topic(),
                &message("completeProvisioning_ack", &Value::Null),
            )
            .await;
        while milestone(&mut milestones).await != Milestone::Provisioned(ProvisioningKind::Deploy) {
        }
        let (task, mut seen) = mock_ac(device, Unit::Responsive);
        // The automatic query, then its ACK and report.
        let (b6, _) = tokio::time::timeout(WAIT, seen.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(b6, B6_QUERY);
        tokio::time::timeout(WAIT, async {
            while ctrl.shared.snapshot().needs_report() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let after = ctrl.apply(off).await.unwrap();
        assert!(after.provisioned);
        assert_eq!(after.state.power, Some(false));
        assert_eq!(
            drain(&mut seen),
            vec![
                (B6_COMMAND, vec![tlv(field::POWER, 0)]),
                (B6_QUERY, vec![tlv(field::QUERY, 2)]),
            ]
        );
        task.abort();
        ctrl.shutdown().await;
    }

    #[test]
    fn sequence_numbers_wrap() {
        let seq = AtomicU8::new(u8::MAX);
        assert_eq!(seq.fetch_add(1, Ordering::Relaxed), u8::MAX);
        assert_eq!(seq.fetch_add(1, Ordering::Relaxed), 0);
    }

    // ---- confirmation (no broker) ----

    const SHORT: Duration = Duration::from_millis(200);

    fn light_on() -> AcCommand {
        AcCommand::Light(true)
    }

    /// Delivers `hex` to `shared` after a short delay, as if it arrived
    /// while a command waits.
    fn later(shared: &Arc<Shared>, hex: String) -> JoinHandle<()> {
        let shared = Arc::clone(shared);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            shared.on_packet(&hex);
        })
    }

    #[tokio::test]
    async fn an_ack_with_seq_0_confirms_the_command() {
        let shared = Arc::new(Shared::new());
        let pending = shared.arm();
        let task = later(&shared, ack_hex());
        assert!(pending.confirmed(&light_on(), WAIT).await.unwrap());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn an_ack_from_before_the_send_is_not_taken() {
        let shared = Arc::new(Shared::new());
        shared.on_packet(&ack_hex());
        let pending = shared.arm();
        assert!(!pending.confirmed(&light_on(), SHORT).await.unwrap());
    }

    #[tokio::test]
    async fn a_report_showing_the_change_confirms_without_an_ack() {
        let shared = Arc::new(Shared::new());
        // A report that does not show it is not enough ...
        let pending = shared.arm();
        let task = later(&shared, report_hex(&[tlv(field::LIGHT, 0)]));
        assert!(!pending.confirmed(&light_on(), SHORT).await.unwrap());
        task.await.unwrap();
        // ... one that does is.
        let pending = shared.arm();
        let task = later(&shared, report_hex(&[tlv(field::LIGHT, 1)]));
        assert!(pending.confirmed(&light_on(), WAIT).await.unwrap());
        task.await.unwrap();
        // A report from before the send does not count either.
        let pending = shared.arm();
        assert!(!pending.confirmed(&light_on(), SHORT).await.unwrap());
    }

    /// A command whose ACK timed out still owes it; when it comes late,
    /// during the next command, it is skipped and only the next ACK counts.
    #[tokio::test]
    async fn a_late_ack_of_a_timed_out_command_does_not_confirm_the_next() {
        let shared = Arc::new(Shared::new());
        let first = shared.arm();
        shared.expect_ack();
        assert!(!first.confirmed(&light_on(), SHORT).await.unwrap());

        // Only the first command's late ACK arrives: not confirmed.
        let second = shared.arm();
        assert_eq!(second.owed, 1);
        shared.expect_ack();
        let task = later(&shared, ack_hex());
        assert!(!second.confirmed(&light_on(), SHORT).await.unwrap());
        task.await.unwrap();

        // The second command's ACK is still owed; a third command skips it
        // and takes the next one.
        let third = shared.arm();
        assert_eq!(third.owed, 1);
        shared.expect_ack();
        let late = later(&shared, ack_hex());
        let own = later(&shared, ack_hex());
        assert!(third.confirmed(&light_on(), WAIT).await.unwrap());
        late.await.unwrap();
        own.await.unwrap();
        assert!(shared.snapshot().unacked.is_empty());
    }

    /// The real unit ACKs a query about a second after its report: a
    /// command sent in between must not take that ACK as its own.
    #[tokio::test]
    async fn a_query_ack_after_its_report_is_not_taken_by_the_next_command() {
        let shared = Arc::new(Shared::new());
        // A query is sent and its report arrives; its ACK has not.
        shared.expect_ack();
        shared.on_packet(&report_hex(&[tlv(field::LIGHT, 0)]));

        let pending = shared.arm();
        assert_eq!(pending.owed, 1);
        shared.expect_ack();
        let task = later(&shared, ack_hex());
        assert!(!pending.confirmed(&light_on(), SHORT).await.unwrap());
        task.await.unwrap();
        // The query is paid; the command's ACK is the one still owed.
        assert_eq!(shared.snapshot().unacked.len(), 1);
    }

    /// Provisioned and owing the query's ACK become visible together, so a
    /// change armed as soon as it may run skips that ACK.
    #[tokio::test]
    async fn provisioning_owes_the_query_ack_at_once() {
        let shared = Arc::new(Shared::new());
        let mut milestones = shared.milestones.subscribe();
        shared.on_connected();
        assert!(!shared.status().provisioned);
        assert_eq!(shared.arm().owed, 0);
        shared.on_provisioned(ProvisioningKind::Deploy);
        assert!(shared.status().provisioned);
        let pending = shared.arm();
        assert_eq!(pending.owed, 1);
        assert_eq!(milestone(&mut milestones).await, Milestone::MqttConnected);
        assert_eq!(
            milestone(&mut milestones).await,
            Milestone::Provisioned(ProvisioningKind::Deploy)
        );
        // The command is lost: the query's ACK alone must not confirm it.
        shared.expect_ack();
        later(&shared, ack_hex());
        assert!(!pending.confirmed(&light_on(), SHORT).await.unwrap());
    }

    #[tokio::test]
    async fn owed_acks_reset_on_reconnect() {
        let shared = Arc::new(Shared::new());
        shared.expect_ack();
        shared.expect_ack();
        assert_eq!(shared.arm().owed, 2);
        shared.on_disconnected();
        assert_eq!(shared.arm().owed, 0);

        shared.expect_ack();
        shared.on_connected();
        let pending = shared.arm();
        assert_eq!(pending.owed, 0);
        shared.expect_ack();
        let task = later(&shared, ack_hex());
        assert!(pending.confirmed(&light_on(), WAIT).await.unwrap());
        task.await.unwrap();
    }

    /// Frames sent long ago are assumed to have lost their ACK.
    #[test]
    fn acks_owed_for_long_ago_are_forgotten() {
        let shared = Shared::new();
        let long_ago = Instant::now()
            .checked_sub(ACK_OWED_FOR + Duration::from_secs(1))
            .unwrap();
        shared.snapshot().unacked.push_back(long_ago);
        shared.expect_ack();
        assert_eq!(shared.arm().owed, 1);
        assert_eq!(shared.snapshot().unacked.len(), 1);
    }

    /// When ACKs are dropped for lag, the command counts as confirmed only
    /// if more were dropped than were owed.
    #[tokio::test]
    async fn lagged_acks_confirm_only_past_those_owed() {
        let shared = Arc::new(Shared::new());
        // More owed than the queue holds; exactly that many arrive: the
        // receiver lags, and every ACK it saw or lost was owed.
        let owed = ACK_QUEUE + 8;
        for _ in 0..owed {
            shared.expect_ack();
        }
        let pending = shared.arm();
        assert_eq!(pending.owed, owed);
        for _ in 0..owed {
            shared.on_packet(&ack_hex());
        }
        assert!(!pending.confirmed(&light_on(), SHORT).await.unwrap());

        // One owed, two lost to lag: ours was among them.
        shared.expect_ack();
        let pending = shared.arm();
        assert_eq!(pending.owed, 1);
        for _ in 0..ACK_QUEUE + 2 {
            shared.on_packet(&ack_hex());
        }
        assert!(pending.confirmed(&light_on(), SHORT).await.unwrap());
    }

    #[test]
    fn reports_confirm_only_what_they_show() {
        let cool_24 = AcCommand::Set {
            mode: AcMode::Cool,
            fan: FanSpeed::Auto,
            setpoint: Setpoint::Temp(temp(24.0)),
        };
        let on_cool_24 = AcCommand::PowerOnWith {
            mode: AcMode::Cool,
            fan: FanSpeed::Auto,
            setpoint: Setpoint::Temp(temp(24.0)),
        };
        let off = AcState {
            power: Some(false),
            ..state(AcMode::Cool, FanSpeed::Auto, 48)
        };
        assert!(confirms(&cool_24, &off));
        assert!(confirms(&AcCommand::PowerOff, &off));
        assert!(!confirms(&on_cool_24, &off), "still off");
        let on = AcState {
            power: Some(true),
            ..off.clone()
        };
        assert!(confirms(&on_cool_24, &on));
        let on_25 = AcState {
            setpoint_raw: Some(50),
            ..on
        };
        assert!(!confirms(&on_cool_24, &on_25), "other setpoint");
        assert!(!confirms(&AcCommand::Query(QueryGroup::State), &on_25));
    }

    // ---- planning (pure) ----

    #[test]
    fn power_on_carries_mode_fan_and_setpoint() {
        let off = AcState {
            power: Some(false),
            ..state(AcMode::Cool, FanSpeed::Low, 42)
        };
        let on = AcChange {
            power: Some(true),
            ..AcChange::default()
        };
        assert_eq!(
            plan_commands(&on, &off).unwrap(),
            vec![AcCommand::PowerOnWith {
                mode: AcMode::Cool,
                fan: FanSpeed::Low,
                setpoint: Setpoint::Temp(temp(21.0))
            }]
        );
        let on_22_light = AcChange {
            temp: Some(temp(22.0)),
            light: Some(true),
            ..on
        };
        assert_eq!(
            plan_commands(&on_22_light, &off).unwrap(),
            vec![
                AcCommand::PowerOnWith {
                    mode: AcMode::Cool,
                    fan: FanSpeed::Low,
                    setpoint: Setpoint::Temp(temp(22.0))
                },
                AcCommand::Light(true)
            ]
        );
        // Without a known state there is nothing to fill in from.
        let err = plan_commands(&on, &AcState::default()).unwrap_err();
        assert!(err.to_string().contains("--mode"), "{err}");
    }

    #[test]
    fn power_off_goes_alone_and_set_never_powers_on() {
        let off = AcState {
            power: Some(false),
            ..state(AcMode::Cool, FanSpeed::Low, 42)
        };
        let power_off = AcChange {
            power: Some(false),
            ..AcChange::default()
        };
        assert_eq!(
            plan_commands(&power_off, &off).unwrap(),
            vec![AcCommand::PowerOff]
        );
        // Power off needs no known state.
        assert_eq!(
            plan_commands(&power_off, &AcState::default()).unwrap(),
            vec![AcCommand::PowerOff]
        );
        let warmer = AcChange {
            temp: Some(temp(25.0)),
            ..AcChange::default()
        };
        assert_eq!(
            plan_commands(&warmer, &off).unwrap(),
            vec![AcCommand::Set {
                mode: AcMode::Cool,
                fan: FanSpeed::Low,
                setpoint: Setpoint::Temp(temp(25.0))
            }]
        );
    }

    fn state(mode: AcMode, fan: FanSpeed, setpoint_raw: u8) -> AcState {
        AcState {
            mode: Some(mode),
            fan: Some(fan),
            setpoint_raw: Some(setpoint_raw),
            ..AcState::default()
        }
    }

    fn temp(c: f32) -> TargetTemp {
        TargetTemp::from_celsius(c).unwrap()
    }

    #[test]
    fn plan_fills_from_state() {
        let cool = state(AcMode::Cool, FanSpeed::Low, 48);
        let set = |change: AcChange, s: &AcState| plan_set(&change, s).unwrap();
        assert_eq!(
            set(
                AcChange {
                    temp: Some(temp(20.0)),
                    ..AcChange::default()
                },
                &cool
            ),
            AcCommand::Set {
                mode: AcMode::Cool,
                fan: FanSpeed::Low,
                setpoint: Setpoint::Temp(temp(20.0))
            }
        );
        assert_eq!(
            set(
                AcChange {
                    fan: Some(FanSpeed::High),
                    ..AcChange::default()
                },
                &cool
            ),
            AcCommand::Set {
                mode: AcMode::Cool,
                fan: FanSpeed::High,
                setpoint: Setpoint::Temp(temp(24.0))
            }
        );
        // In Auto the reported offset is kept; leaving Auto needs a temperature.
        let auto = state(AcMode::Auto, FanSpeed::Auto, 36);
        assert_eq!(
            set(
                AcChange {
                    fan: Some(FanSpeed::Mid),
                    ..AcChange::default()
                },
                &auto
            ),
            AcCommand::Set {
                mode: AcMode::Auto,
                fan: FanSpeed::Mid,
                setpoint: Setpoint::Offset(ComfortOffset::new(1).unwrap())
            }
        );
        let err = plan_set(
            &AcChange {
                mode: Some(AcMode::Dry),
                ..AcChange::default()
            },
            &auto,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--temp"), "{err}");
        let err = plan_set(
            &AcChange {
                offset: Some(ComfortOffset::new(1).unwrap()),
                ..AcChange::default()
            },
            &cool,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a comfort offset"), "{err}");
        let err = plan_set(
            &AcChange {
                temp: Some(temp(25.0)),
                ..AcChange::default()
            },
            &AcState::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--mode"), "{err}");
    }

    #[test]
    fn impossible_changes_are_refused_up_front() {
        let off = Some(false);
        for (change, expected) in [
            (AcChange::default(), "nothing to change"),
            (
                AcChange {
                    power: off,
                    temp: Some(temp(24.0)),
                    ..AcChange::default()
                },
                "power off cannot be combined",
            ),
            (
                AcChange {
                    temp: Some(temp(24.0)),
                    offset: Some(ComfortOffset::new(0).unwrap()),
                    ..AcChange::default()
                },
                "not both",
            ),
            (
                AcChange {
                    mode: Some(AcMode::Auto),
                    temp: Some(temp(24.0)),
                    ..AcChange::default()
                },
                "auto mode takes a comfort offset",
            ),
            (
                AcChange {
                    mode: Some(AcMode::Fan),
                    offset: Some(ComfortOffset::new(0).unwrap()),
                    ..AcChange::default()
                },
                "fan mode takes a target temperature",
            ),
        ] {
            let err = change.check().unwrap_err();
            assert!(err.to_string().contains(expected), "{change:?}: {err}");
        }
        AcChange {
            power: off,
            light: Some(true),
            ..AcChange::default()
        }
        .check()
        .unwrap();
    }
}
