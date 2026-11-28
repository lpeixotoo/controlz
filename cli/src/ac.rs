//! `controlz ac ...`: clap glue over `controlz_core::ac`.
//!
//! `setup` provisions the module (or, with `--info-only`, only reads it) and
//! continues as `serve`; `serve` runs the local cloud, the broker and the IPC
//! socket in the foreground; `status` and `set` talk to that serve over IPC.
//! With `--dns`, `setup` and `serve` also run the provisioning DNS fallback
//! (`controlz_core::ac::dns`) for as long as they run.

use std::fmt::Write as _;
use std::io::{BufRead as _, IsTerminal as _};
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use controlz_core::ac::cloud::{API_PORT, CloudConfig, MQTT_PORT};
use controlz_core::ac::controller::{AcChange, AcController, AcStatus, Milestone, ServeConfig};
use controlz_core::ac::dns::{self, DnsConfig, DnsServer, NameRule};
use controlz_core::ac::mqtt::Pinning;
use controlz_core::ac::pki::{ModulePins, Pki};
use controlz_core::ac::setup::{self, DeviceInfo, ProvisionRequest, Release};
use controlz_core::ac::tlv::{AcMode, AcState, ComfortOffset, FanSpeed, TargetTemp};
use controlz_core::{Config, Secret};
use tokio::net::UnixListener;
use tokio::sync::broadcast::error::RecvError;

use crate::ac_ipc;
use crate::secrets::Error;

/// How long `setup` waits for this Mac to get its home address back.
const ADDRESS_WAIT: Duration = Duration::from_mins(3);

#[derive(Debug, Subcommand)]
pub enum AcCmd {
    /// Provision the AC's Wi-Fi module (hotspot mode) to this Mac, then serve it
    Setup(SetupArgs),
    /// Run the local cloud and MQTT broker for the provisioned AC (foreground)
    Serve(ServeArgs),
    /// Show the AC's state (needs `controlz ac serve` running)
    Status,
    /// Change the AC's state and print the state it reports back
    Set(SetArgs),
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Only read the module's device info; sends nothing that provisions it
    #[arg(long)]
    info_only: bool,
    /// The module's address on its LG… network (its gateway)
    #[arg(long)]
    host: String,
    /// The module's setup port
    #[arg(long, default_value_t = setup::SETUP_PORT)]
    port: u16,
    /// Also print identifiers (uuid, MAC, …); keep them out of the repo
    #[arg(long)]
    show_ids: bool,
    /// Send `setDeviceInit` before `getDeviceInfo` (retry if the module refuses)
    #[arg(long)]
    init: bool,
    /// The home Wi-Fi network the AC joins; its password is read from piped stdin
    #[arg(long, required_unless_present = "info_only")]
    ssid: Option<String>,
    /// What the AC is told to call this Mac: `ip` (ac.bind as is),
    /// `sslip` (<ip-with-dashes>.sslip.io) or a fully qualified DNS name
    #[arg(long, default_value = "ip")]
    hostname: String,
    #[command(flatten)]
    dns: DnsArgs,
}

#[derive(Debug, Default, Args)]
pub struct ServeArgs {
    #[command(flatten)]
    dns: DnsArgs,
    /// Pin the AC's own (LG-issued) client certificate key on its next
    /// connect if none is pinned yet; `--dns` implies it
    #[arg(long)]
    pin: bool,
}

/// The provisioning DNS fallback flags, shared by `setup` and `serve`.
#[derive(Debug, Clone, PartialEq, Args)]
pub struct DnsArgs {
    /// Also answer DNS on UDP/TCP 53: LG's setup names (*.lgthinq.com) resolve
    /// to ac.bind, the rest is forwarded; needs the router steps it prints
    #[arg(long)]
    dns: bool,
    /// Where `--dns` forwards every other name (port 53)
    #[arg(long, default_value_t = dns::DEFAULT_UPSTREAM, requires = "dns")]
    dns_upstream: IpAddr,
    /// One more name `--dns` answers with ac.bind (repeatable; a leading `.`
    /// means every name below that domain)
    #[arg(long = "dns-name", value_name = "NAME", requires = "dns")]
    dns_names: Vec<String>,
}

impl Default for DnsArgs {
    fn default() -> Self {
        Self {
            dns: false,
            dns_upstream: dns::DEFAULT_UPSTREAM,
            dns_names: Vec::new(),
        }
    }
}

/// `ac set` flags; also parsed by `serve` from the IPC request.
#[derive(Debug, Clone, Default, PartialEq, Args)]
pub struct SetArgs {
    /// on | off (power is sent alone, before anything else)
    #[arg(long, value_parser = parse_on_off)]
    power: Option<bool>,
    /// cool | dry | fan | auto
    #[arg(long)]
    mode: Option<AcMode>,
    /// Target temperature in °C, 18–30 in 0.5 steps (not in auto)
    #[arg(long)]
    temp: Option<f32>,
    /// low | low-mid | mid | mid-high | high | auto
    #[arg(long)]
    fan: Option<FanSpeed>,
    /// Comfort offset in auto mode, -2..=2
    #[arg(long, allow_negative_numbers = true)]
    offset: Option<i8>,
    /// Display light: on | off
    #[arg(long, value_parser = parse_on_off)]
    light: Option<bool>,
}

impl SetArgs {
    fn to_change(&self) -> Result<AcChange, controlz_core::ControlError> {
        Ok(AcChange {
            power: self.power,
            mode: self.mode,
            temp: self.temp.map(TargetTemp::from_celsius).transpose()?,
            fan: self.fan,
            offset: self.offset.map(ComfortOffset::new).transpose()?,
            light: self.light,
        })
    }

    /// The flags again, as `serve` parses them from the IPC request.
    fn to_argv(&self) -> Vec<String> {
        let on_off = |b: bool| if b { "on" } else { "off" }.to_owned();
        let mut argv = vec!["set".to_owned()];
        let mut push = |flag: &str, value: Option<String>| {
            if let Some(value) = value {
                argv.extend([format!("--{flag}"), value]);
            }
        };
        push("power", self.power.map(on_off));
        push("mode", self.mode.map(|m| m.to_string()));
        push("temp", self.temp.map(|t| t.to_string()));
        push("fan", self.fan.map(|f| f.to_string()));
        push("offset", self.offset.map(|o| o.to_string()));
        push("light", self.light.map(on_off));
        argv
    }
}

fn parse_on_off(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(format!("expected on or off, got {s:?}")),
    }
}

/// What `serve` accepts over IPC: the same clap types as the CLI.
#[derive(Debug, Parser)]
#[command(name = "ac", disable_help_subcommand = true)]
struct IpcRequest {
    #[command(subcommand)]
    cmd: IpcCmd,
}

#[derive(Debug, Subcommand)]
enum IpcCmd {
    Status,
    Set(SetArgs),
}

/// `getDeviceInfo` fields that identify nothing; printed without `--show-ids`.
const PLAIN_FIELDS: &[&str] = &[
    "agentVer",
    "countryCode",
    "demandType",
    "deviceType",
    "errorCode",
    "errorcodeDisplay",
    "factoryResetCode",
    "isReg",
    "mdFota",
    "modemVer",
    "protocolVer",
    "subErrorCode",
    "supportsWpa3",
];

pub fn run(cmd: AcCmd) -> Result<(), Error> {
    match cmd {
        AcCmd::Setup(args) => run_setup(&args),
        AcCmd::Serve(args) => run_serve(&args),
        AcCmd::Status => run_client(&["status".to_owned()]),
        AcCmd::Set(args) => {
            // Fail on bad values here rather than after a round trip.
            args.to_change()?.check()?;
            run_client(&args.to_argv())
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?)
}

fn run_setup(args: &SetupArgs) -> Result<(), Error> {
    if args.info_only {
        let info =
            runtime()?.block_on(setup::read_device_info(&args.host, args.port, args.init))?;
        print!("{}", render(&info, args.show_ids));
        return Ok(());
    }
    let ssid = args
        .ssid
        .as_deref()
        .ok_or("--ssid is required for a full setup")?;
    let path = Config::default_path()?;
    let mut cfg = Config::load_from(&path)?;
    let bind = configured_bind(&cfg)?;
    let host = advertised_host(&args.hostname, bind)?;
    let cloud = CloudConfig {
        bind,
        port: API_PORT,
        advertised_host: host.clone(),
    };
    cloud.validate()?;
    let dns_config = dns_config(&args.dns, bind, &host)?;
    // Before touching the module, so a PKI problem or an old serve still
    // holding the ports and the socket cannot strand it.
    let pki_dir = Pki::default_dir()?;
    let pki = Pki::load_or_create(&pki_dir, &host)?;
    let runtime = runtime()?;
    let listener = runtime.block_on(ac_ipc::bind(&ac_ipc::default_socket_path()?))?;
    // Also before the module: a busy or refused port 53 aborts here. Kept
    // alive (it is dropped at return) through the wait and the serve below.
    let _dns = start_dns(&runtime, dns_config.as_ref(), RouterSteps::Setup)?;
    let request = ProvisionRequest {
        api_server: cloud.api_server(),
        mqtt_server: cloud.mqtt_server(),
        ssid: ssid.to_owned(),
        password: read_password_from_stdin()?,
        init: args.init,
    };
    // The host is this Mac's IP (or derived from it): keep it off the screen.
    println!(
        "setup: provisioning the module (hostname strategy: {})",
        args.hostname
    );
    let mut saved = false;
    let save = |info: &DeviceInfo| {
        let uuid = info.uuid.as_deref().unwrap_or_default();
        cfg.set("ac.uuid", uuid)?;
        cfg.set("ac.host", &host)?;
        cfg.save_to(&path)?;
        saved = true;
        println!("setup: ac.uuid and ac.host saved");
        Ok(())
    };
    let done = match runtime.block_on(setup::provision(&args.host, args.port, &request, save)) {
        Ok(done) => done,
        Err(e) if saved => {
            return Err(format!(
                "{e}\nsetup: ac.uuid and ac.host were saved; if the AC left its LG hotspot, \
                 rejoin the home Wi-Fi and run `controlz ac serve --pin`"
            )
            .into());
        }
        Err(e) => return Err(e.into()),
    };
    let uuid = done
        .info
        .uuid
        .clone()
        .ok_or("getDeviceInfo returned no uuid")?;
    match &done.release {
        Release::Acknowledged => {
            println!("setup: {} provisioned and released", done.info.model_name);
        }
        Release::NoReply { reason } => println!(
            "setup: {} got the Wi-Fi settings; no confirmation ({reason}), \
             assuming it left its hotspot",
            done.info.model_name
        ),
    }
    println!(
        "setup: switch this Mac back to the home Wi-Fi now; waiting up to {} min for ac.bind",
        ADDRESS_WAIT.as_secs() / 60
    );
    runtime.block_on(wait_for_address(bind, ADDRESS_WAIT))?;
    println!("setup: ac.bind is up; serving");
    // A setup is always a pinning window (D21).
    let config = serve_config(bind, host, &pki_dir, true);
    runtime.block_on(serve(config, pki, uuid, listener))
}

fn configured_bind(cfg: &Config) -> Result<IpAddr, Error> {
    let bind = cfg.get("ac.bind")?.ok_or(
        "ac.bind is not set; on the home Wi-Fi run: \
         controlz config set ac.bind \"$(ipconfig getifaddr en0)\"",
    )?;
    Ok(bind.parse()?)
}

/// The host the module is told to use, from `--hostname`.
fn advertised_host(hostname: &str, bind: IpAddr) -> Result<String, Error> {
    match hostname {
        "ip" => Ok(bind.to_string()),
        "sslip" => match bind {
            IpAddr::V4(v4) => Ok(format!("{}.sslip.io", v4.to_string().replace('.', "-"))),
            IpAddr::V6(_) => Err("--hostname sslip needs an IPv4 ac.bind".into()),
        },
        name if is_fqdn(name) => Ok(name.to_ascii_lowercase()),
        name => Err(format!(
            "--hostname {name:?} is not ip, sslip or a fully qualified DNS name \
             (like ac.example.org)"
        )
        .into()),
    }
}

/// A DNS name with at least two labels of letters, digits and inner
/// hyphens, whose last label has a letter. Catches typos like `sslio`
/// before they are written into the module.
fn is_fqdn(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |l: &&str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    name.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(label_ok)
        && labels
            .last()
            .is_some_and(|l| l.bytes().any(|b| b.is_ascii_alphabetic()))
}

/// Reads the Wi-Fi password from piped stdin; refuses a terminal (it would
/// echo into scrollback).
fn read_password_from_stdin() -> Result<Secret, Error> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(
            "refusing to read the Wi-Fi password from the terminal (it would echo); \
             pipe it in: read -rs PW; printf '%s\\n' \"$PW\" | controlz ac setup ..."
                .into(),
        );
    }
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let value = line.trim_end_matches(['\n', '\r']);
    if value.is_empty() {
        return Err("no Wi-Fi password on stdin".into());
    }
    Ok(Secret::new(value))
}

/// Waits until `ip` is assigned to one of this Mac's interfaces (binding
/// it succeeds only then).
async fn wait_for_address(ip: IpAddr, limit: Duration) -> Result<(), Error> {
    let start = Instant::now();
    loop {
        if std::net::TcpListener::bind((ip, 0)).is_ok() {
            return Ok(());
        }
        if start.elapsed() >= limit {
            return Err(format!(
                "this Mac did not get ac.bind back within {} s; \
                 rejoin the home Wi-Fi, then run `controlz ac serve`",
                limit.as_secs()
            )
            .into());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn run_serve(args: &ServeArgs) -> Result<(), Error> {
    let cfg = Config::load_from(&Config::default_path()?)?;
    let missing = "not set; run `controlz ac setup` first";
    let uuid = cfg
        .get("ac.uuid")?
        .ok_or_else(|| format!("ac.uuid {missing}"))?;
    let host = cfg
        .get("ac.host")?
        .ok_or_else(|| format!("ac.host {missing}"))?;
    let bind = configured_bind(&cfg)?;
    let dns_config = dns_config(&args.dns, bind, &host)?;
    let pki_dir = Pki::default_dir()?;
    let pki = Pki::load_or_create(&pki_dir, &host)?;
    let runtime = runtime()?;
    let listener = runtime.block_on(ac_ipc::bind(&ac_ipc::default_socket_path()?))?;
    let _dns = start_dns(&runtime, dns_config.as_ref(), RouterSteps::Serve)?;
    let window = pin_window(args);
    let config = serve_config(bind, host, &pki_dir, window);
    runtime.block_on(serve(config, pki, uuid, listener))
}

/// Whether `serve` may pin a module certificate we did not issue (D21):
/// with `--pin`, or with `--dns` (a provisioning session).
fn pin_window(args: &ServeArgs) -> bool {
    args.pin || args.dns.dns
}

/// The production serve config with module pins in `pki_dir` (D21).
fn serve_config(bind: IpAddr, host: String, pki_dir: &Path, window: bool) -> ServeConfig {
    if window {
        println!(
            "serve: pinning window open: an AC with its own (LG) client certificate gets its \
             key pinned on its first connect"
        );
    }
    let mut config = ServeConfig::new(bind, host);
    config.pinning = Some(Pinning {
        pins: ModulePins::new(pki_dir),
        window,
    });
    config
}

/// The resolver config for `--dns`: the default names, the advertised host
/// when it is a DNS name, and every `--dns-name`; `None` without `--dns`.
fn dns_config(args: &DnsArgs, bind: IpAddr, host: &str) -> Result<Option<DnsConfig>, Error> {
    if !args.dns {
        return Ok(None);
    }
    let mut config = DnsConfig::new(bind, args.dns_upstream);
    if host.parse::<IpAddr>().is_err() {
        config.overrides.push(NameRule::parse(host)?);
    }
    for name in &args.dns_names {
        config.overrides.push(NameRule::parse(name)?);
    }
    let mut seen = std::collections::HashSet::new();
    config.overrides.retain(|rule| seen.insert(rule.clone()));
    config.validate()?;
    Ok(Some(config))
}

/// Which command prints the router steps (the order differs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouterSteps {
    Setup,
    Serve,
}

/// Starts the resolver for `--dns` and prints what the user must do on the
/// router.
fn start_dns(
    runtime: &tokio::runtime::Runtime,
    config: Option<&DnsConfig>,
    steps: RouterSteps,
) -> Result<Option<DnsServer>, Error> {
    let Some(config) = config else {
        return Ok(None);
    };
    let server = runtime.block_on(DnsServer::start(config))?;
    let names: Vec<String> = config.overrides.iter().map(ToString::to_string).collect();
    // ac.bind is this Mac's IP: keep it off the screen (it ends up in issues).
    println!(
        "dns: answering {} with ac.bind on UDP/TCP :{}; forwarding the rest to {}",
        names.join(", "),
        server.listen_addr().port(),
        config.upstream
    );
    print!("{}", router_steps(steps));
    Ok(Some(server))
}

/// The router procedure (KB: Home network, provisioning DNS).
fn router_steps(steps: RouterSteps) -> String {
    let point = "set the router's DHCP \"specific DNS server\" (Servidor DNS Específico) \
                 to ac.bind (this Mac's address) and save";
    let revert = "once the AC fetched /route and its certificate (the route: and cert: \
                  lines below), set the router's DNS back to automatic and save";
    let lease = "devices that renewed their DHCP lease meanwhile keep this Mac as DNS \
                 until the router's lease time runs out: keep this command running that \
                 long, or reconnect them (Wi-Fi off/on)";
    let watch = "run with -v to see the dns: query lines";
    match steps {
        RouterSteps::Setup => format!(
            "dns: router steps (yours; {watch}):\n\
             \x20 1. before this setup: {point}\n\
             \x20 2. when this Mac is back on the home Wi-Fi and no lgthinq.com query \
             shows up within a minute, power-cycle the AC (plug or breaker)\n\
             \x20 3. {revert}\n\
             \x20 4. {lease}\n"
        ),
        RouterSteps::Serve => format!(
            "dns: router steps (yours; {watch}):\n\
             \x20 1. now: {point}\n\
             \x20 2. power-cycle the AC (plug or breaker) so it renews its lease; if no \
             lgthinq.com query shows up within a minute, power-cycle it again\n\
             \x20 3. {revert}\n\
             \x20 4. {lease}\n"
        ),
    }
}

/// Runs the controller and serves IPC on `listener` until the process is
/// stopped.
async fn serve(
    config: ServeConfig,
    pki: Pki,
    uuid: String,
    listener: UnixListener,
) -> Result<(), Error> {
    let controller = Arc::new(AcController::start(&config, &pki, &uuid).await?);
    let mut milestones = controller.milestones();
    let start = Instant::now();
    println!(
        "serve: HTTPS :{API_PORT}, MQTT :{MQTT_PORT} on ac.bind, advertising ac.host; \
         waiting for the AC (Ctrl-C stops)"
    );
    loop {
        tokio::select! {
            milestone = milestones.recv() => match milestone {
                Ok(m) => println!("[+{}s] {}", start.elapsed().as_secs(), render_milestone(&m)),
                Err(RecvError::Lagged(n)) => println!("({n} progress lines skipped)"),
                Err(RecvError::Closed) => return Err("the controller stopped".into()),
            },
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let controller = Arc::clone(&controller);
                    tokio::spawn(ac_ipc::serve_one(stream, move |args| handle(controller, args)));
                }
                Err(e) => tracing::warn!(error = %e, "ipc: accept failed"),
            },
        }
    }
}

/// Runs one IPC request against the controller.
async fn handle(controller: Arc<AcController>, args: Vec<String>) -> Result<String, String> {
    let request = IpcRequest::try_parse_from(std::iter::once("ac".to_owned()).chain(args))
        .map_err(|e| e.to_string())?;
    let status = match request.cmd {
        IpcCmd::Status => controller.status().await,
        IpcCmd::Set(args) => {
            let change = args.to_change().map_err(|e| e.to_string())?;
            controller.apply(change).await
        }
    };
    status.map(|s| render_status(&s)).map_err(|e| e.to_string())
}

fn run_client(args: &[String]) -> Result<(), Error> {
    let socket = ac_ipc::default_socket_path()?;
    let reply = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(ac_ipc::request(&socket, args))?;
    print!("{reply}");
    Ok(())
}

fn render_milestone(m: &Milestone) -> String {
    match m {
        Milestone::RouteServed => "route: the AC fetched /route".to_owned(),
        Milestone::CaServed => "route: the AC fetched our CA".to_owned(),
        Milestone::ClientCertIssued => "cert: client certificate issued".to_owned(),
        Milestone::MqttConnected => "mqtt: the AC connected".to_owned(),
        Milestone::Provisioned(kind) => format!("mqtt: provisioned ({kind:?})"),
        Milestone::MqttDisconnected => "mqtt: the AC disconnected".to_owned(),
        Milestone::State(state) => format!("state: {}", summary(state)),
        other => format!("{other:?}"),
    }
}

fn on_off(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "on",
        Some(false) => "off",
        None => "?",
    }
}

/// One line: power, mode, setpoint, fan, room temperature.
fn summary(s: &AcState) -> String {
    let show = |v: Option<String>| v.unwrap_or_else(|| "?".to_owned());
    format!(
        "power {}, mode {}, {}, fan {}, room {}",
        on_off(s.power),
        show(s.mode.map(|m| m.to_string())),
        setpoint(s),
        show(s.fan.map(|f| f.to_string())),
        show(s.room_temp.map(|t| format!("{:.1} °C", t.celsius()))),
    )
}

fn setpoint(s: &AcState) -> String {
    if let Some(t) = s.target_temp() {
        format!("target {:.1} °C", t.celsius())
    } else if let Some(o) = s.comfort_offset() {
        format!("offset {:+}", o.get())
    } else {
        "target ?".to_owned()
    }
}

fn render_status(s: &AcStatus) -> String {
    let mut out = String::new();
    let link = match (s.connected, s.provisioned) {
        (true, true) => "connected",
        (true, false) => "connected (not provisioned in this session yet)",
        (false, _) => "not connected to this Mac",
    };
    let _ = writeln!(out, "AC: {link}");
    match s.last_report_age {
        None => {
            let _ = writeln!(out, "no state report yet");
        }
        Some(age) => {
            let st = &s.state;
            let _ = writeln!(out, "power: {}", on_off(st.power));
            let mode = st.mode.map_or_else(|| "?".to_owned(), |m| m.to_string());
            let _ = writeln!(out, "mode: {mode}");
            let _ = writeln!(out, "{}", setpoint(st).replacen(' ', ": ", 1));
            let fan = st.fan.map_or_else(|| "?".to_owned(), |f| f.to_string());
            let _ = writeln!(out, "fan: {fan}");
            let room = st
                .room_temp
                .map_or_else(|| "?".to_owned(), |t| format!("{:.1} °C", t.celsius()));
            let _ = writeln!(out, "room: {room}");
            let _ = writeln!(out, "light: {}", on_off(st.light));
            let _ = writeln!(out, "last report: {} s ago", age.as_secs());
        }
    }
    out
}

fn render(info: &DeviceInfo, show_ids: bool) -> String {
    let mut out = String::new();
    let none = "(missing)";
    let _ = writeln!(out, "modelName: {}", info.model_name);
    let _ = writeln!(
        out,
        "softwareVer: {}",
        info.software_ver.as_deref().unwrap_or(none)
    );
    let _ = match &info.encrypt_val {
        Some(v) => writeln!(out, "encrypt_val: present ({} chars)", v.len()),
        None => writeln!(out, "encrypt_val: {none}"),
    };
    let _ = writeln!(out, "extra: {}", info.extra.as_deref().unwrap_or(none));
    let mut hidden = Vec::new();
    for (key, value) in &info.other {
        if show_ids || PLAIN_FIELDS.contains(&key.as_str()) {
            let _ = writeln!(out, "{key}: {value}");
        } else {
            hidden.push(key.as_str());
        }
    }
    if show_ids {
        let _ = writeln!(out, "uuid: {}", info.uuid.as_deref().unwrap_or(none));
    } else {
        if info.uuid.is_some() {
            hidden.insert(0, "uuid");
        }
        if !hidden.is_empty() {
            let _ = writeln!(out, "hidden (use --show-ids): {}", hidden.join(", "));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const FAKE_UUID: &str = "00000000-0000-0000-0000-000000000000";

    fn info() -> DeviceInfo {
        DeviceInfo {
            model_name: "RAC_000000_WW".to_owned(),
            uuid: Some(FAKE_UUID.to_owned()),
            software_ver: Some("000000".to_owned()),
            encrypt_val: Some("ZmFrZQ==".to_owned()),
            extra: Some("FAKE|encryptRes:0".to_owned()),
            other: BTreeMap::from([
                ("protocolVer".to_owned(), "4.5".to_owned()),
                ("mac".to_owned(), "xx:xx:xx:xx:xx:xx".to_owned()),
            ]),
        }
    }

    #[test]
    fn render_hides_ids_by_default() {
        let out = render(&info(), false);
        assert!(out.contains("modelName: RAC_000000_WW"), "{out}");
        assert!(out.contains("protocolVer: 4.5"), "{out}");
        assert!(out.contains("encrypt_val: present (8 chars)"), "{out}");
        assert!(out.contains("hidden (use --show-ids): uuid, mac"), "{out}");
        for secret in [FAKE_UUID, "xx:xx", "ZmFrZQ=="] {
            assert!(!out.contains(secret), "leaked {secret}: {out}");
        }
    }

    #[test]
    fn render_shows_ids_on_request() {
        let out = render(&info(), true);
        assert!(out.contains(FAKE_UUID), "{out}");
        assert!(out.contains("mac: xx:xx:xx:xx:xx:xx"), "{out}");
        assert!(
            !out.contains("ZmFrZQ=="),
            "encrypt_val value printed: {out}"
        );
        assert!(!out.contains("hidden"), "{out}");
    }

    #[test]
    fn full_setup_needs_an_ssid_before_reading_anything() {
        let args = SetupArgs {
            info_only: false,
            host: "controlz.test".to_owned(),
            port: setup::SETUP_PORT,
            show_ids: false,
            init: false,
            ssid: None,
            hostname: "ip".to_owned(),
            dns: DnsArgs::default(),
        };
        let err = run_setup(&args).unwrap_err();
        assert!(err.to_string().contains("--ssid"), "{err}");
    }

    #[test]
    fn hostname_strategies() {
        let v4: IpAddr = "192.0.2.5".parse().unwrap(); // privacy:allow (TEST-NET-1)
        assert_eq!(advertised_host("ip", v4).unwrap(), "192.0.2.5"); // privacy:allow
        assert_eq!(advertised_host("sslip", v4).unwrap(), "192-0-2-5.sslip.io");
        assert_eq!(
            advertised_host("ac.example.org", v4).unwrap(),
            "ac.example.org"
        );
        assert_eq!(
            advertised_host("AC.Example.org", v4).unwrap(),
            "ac.example.org"
        );
        for typo in [
            "sslio",
            "IP",
            "localhost",
            "192.0.2.5", // privacy:allow (TEST-NET-1)
            "ac..example.org",
            "-ac.example.org",
            "ac.example.org.",
            "ac_1.example.org",
            "https://ac.example.org",
        ] {
            let err = advertised_host(typo, v4).unwrap_err();
            assert!(err.to_string().contains("fully qualified"), "{typo}: {err}");
        }
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(advertised_host("ip", v6).unwrap(), "2001:db8::1");
        assert!(advertised_host("sslip", v6).is_err());
    }

    #[test]
    fn dns_config_from_flags() {
        let v4: IpAddr = "192.0.2.5".parse().unwrap(); // privacy:allow (TEST-NET-1)
        assert_eq!(
            dns_config(&DnsArgs::default(), v4, "ac.example.org").unwrap(),
            None
        );
        let args = DnsArgs {
            dns: true,
            dns_upstream: "192.0.2.1".parse().unwrap(), // privacy:allow (TEST-NET-1)
            dns_names: vec![
                ".iot.example.net".to_owned(),
                "Common.LGThinQ.com".to_owned(),
            ],
        };
        let config = dns_config(&args, v4, "ac.example.org").unwrap().unwrap();
        let names: Vec<String> = config.overrides.iter().map(ToString::to_string).collect();
        assert_eq!(
            names,
            [
                "common.lgthinq.com",
                "comon.lgthinq.com",
                ".lgthinq.com",
                "ac.example.org",
                ".iot.example.net",
            ]
        );
        assert_eq!(config.bind, v4);
        assert_eq!(config.port, 53);
        assert_eq!(config.upstream.to_string(), "192.0.2.1:53"); // privacy:allow (TEST-NET-1)
        let by_ip = dns_config(&args, v4, "192.0.2.5").unwrap().unwrap(); // privacy:allow
        assert_eq!(by_ip.overrides.len(), 4, "an IP host adds no name");
        let bad = DnsArgs {
            dns_names: vec!["not a name".to_owned()],
            ..args.clone()
        };
        assert!(dns_config(&bad, v4, "ac.example.org").is_err());
        let looped = DnsArgs {
            dns_upstream: v4,
            ..args
        };
        assert!(dns_config(&looped, v4, "ac.example.org").is_err());
    }

    #[derive(Debug, Parser)]
    struct AcCli {
        #[command(subcommand)]
        cmd: AcCmd,
    }

    fn parse_ac(args: &[&str]) -> Result<AcCmd, clap::Error> {
        AcCli::try_parse_from([&["ac"], args].concat()).map(|c| c.cmd)
    }

    #[test]
    fn serve_pins_with_pin_or_dns_only() {
        let window = |args: &[&str]| {
            let AcCmd::Serve(serve) = parse_ac(args).unwrap() else {
                panic!("not serve");
            };
            pin_window(&serve)
        };
        assert!(!window(&["serve"]));
        assert!(window(&["serve", "--pin"]));
        assert!(window(&["serve", "--dns"]));
        assert!(window(&["serve", "--dns", "--pin"]));
        let config = serve_config(
            IpAddr::from([192, 0, 2, 7]), // privacy:allow (TEST-NET-1)
            "controlz.test".to_owned(),
            Path::new("/nonexistent/pki"),
            false,
        );
        assert_eq!(
            config.pinning,
            Some(Pinning {
                pins: ModulePins::new("/nonexistent/pki"),
                window: false,
            })
        );
    }

    #[test]
    fn dns_flags_parse_on_setup_and_serve() {
        let AcCmd::Serve(plain) = parse_ac(&["serve"]).unwrap() else {
            panic!("not serve");
        };
        assert_eq!(plain.dns, DnsArgs::default());
        let AcCmd::Serve(with) = parse_ac(&[
            "serve",
            "--dns",
            "--dns-upstream",
            "192.0.2.1", // privacy:allow (TEST-NET-1)
            "--dns-name",
            ".iot.example.net",
            "--dns-name",
            "a.example.org",
        ])
        .unwrap() else {
            panic!("not serve");
        };
        assert!(with.dns.dns);
        assert_eq!(with.dns.dns_upstream.to_string(), "192.0.2.1"); // privacy:allow
        assert_eq!(with.dns.dns_names, [".iot.example.net", "a.example.org"]);
        let AcCmd::Setup(setup) = parse_ac(&[
            "setup",
            "--host",
            "controlz.test",
            "--ssid",
            "fake-ssid",
            "--dns",
        ])
        .unwrap() else {
            panic!("not setup");
        };
        assert!(setup.dns.dns);
        assert_eq!(setup.dns.dns_upstream, dns::DEFAULT_UPSTREAM);
        for needs_dns in [
            &["serve", "--dns-name", "a.example.org"][..],
            &["serve", "--dns-upstream", "192.0.2.1"], // privacy:allow (TEST-NET-1)
        ] {
            assert!(parse_ac(needs_dns).is_err(), "{needs_dns:?}");
        }
        assert!(parse_ac(&["serve", "--dns", "--dns-upstream", "not-an-ip"]).is_err());
    }

    #[test]
    fn router_steps_say_set_revert_and_power_cycle() {
        for steps in [RouterSteps::Setup, RouterSteps::Serve] {
            let text = router_steps(steps);
            for needle in [
                "specific DNS server",
                "back to automatic",
                "power-cycle",
                "lease",
            ] {
                assert!(text.contains(needle), "{steps:?} lacks {needle}: {text}");
            }
            assert_eq!(text.lines().count(), 5, "{text}");
        }
        assert!(router_steps(RouterSteps::Setup).contains("before this setup"));
        assert!(router_steps(RouterSteps::Serve).contains("1. now"));
    }

    fn parse_ipc(argv: &[String]) -> IpcCmd {
        IpcRequest::try_parse_from(std::iter::once("ac".to_owned()).chain(argv.iter().cloned()))
            .unwrap()
            .cmd
    }

    #[test]
    fn set_args_round_trip_through_ipc() {
        let wanted = SetArgs {
            power: Some(true),
            mode: Some(AcMode::Auto),
            temp: None,
            fan: Some(FanSpeed::LowMid),
            offset: Some(-1),
            light: Some(false),
        };
        let argv = wanted.to_argv();
        assert_eq!(
            argv,
            [
                "set", "--power", "on", "--mode", "auto", "--fan", "low-mid", "--offset", "-1",
                "--light", "off"
            ]
        );
        let IpcCmd::Set(parsed) = parse_ipc(&argv) else {
            panic!("not a set");
        };
        assert_eq!(parsed, wanted);

        let temp = SetArgs {
            temp: Some(22.5),
            ..SetArgs::default()
        };
        let IpcCmd::Set(parsed) = parse_ipc(&temp.to_argv()) else {
            panic!("not a set");
        };
        assert_eq!(parsed, temp);
        assert_eq!(
            parsed
                .to_change()
                .unwrap()
                .temp
                .map(TargetTemp::half_degrees),
            Some(45)
        );
        assert!(matches!(parse_ipc(&["status".to_owned()]), IpcCmd::Status));
        assert!(
            IpcRequest::try_parse_from(["ac", "serve"]).is_err(),
            "only status and set over IPC"
        );
    }

    #[test]
    fn bad_set_values_are_refused() {
        for (temp, offset) in [(Some(31.0), None), (Some(22.3), None), (None, Some(3))] {
            let args = SetArgs {
                temp,
                offset,
                ..SetArgs::default()
            };
            assert!(args.to_change().is_err(), "{args:?}");
        }
        assert_eq!(parse_on_off("ON"), Ok(true));
        assert!(parse_on_off("maybe").is_err());
    }

    #[test]
    fn status_renders_state_and_link() {
        let mut state = AcState {
            power: Some(true),
            mode: Some(AcMode::Cool),
            fan: Some(FanSpeed::Auto),
            setpoint_raw: Some(48),
            light: Some(true),
            ..AcState::default()
        };
        let status = AcStatus {
            state: state.clone(),
            connected: true,
            provisioned: true,
            last_report_age: Some(Duration::from_secs(3)),
        };
        assert_eq!(
            render_status(&status),
            "AC: connected\npower: on\nmode: cool\ntarget: 24.0 °C\nfan: auto\nroom: ?\n\
             light: on\nlast report: 3 s ago\n"
        );
        state.mode = Some(AcMode::Auto);
        state.setpoint_raw = Some(36);
        assert_eq!(
            summary(&state),
            "power on, mode auto, offset +1, fan auto, room ?"
        );
        let offline = AcStatus {
            state: AcState::default(),
            connected: false,
            provisioned: false,
            last_report_age: None,
        };
        assert_eq!(
            render_status(&offline),
            "AC: not connected to this Mac\nno state report yet\n"
        );
    }
}
