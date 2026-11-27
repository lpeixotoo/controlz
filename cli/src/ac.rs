//! `controlz ac ...`: clap glue over `controlz_core::ac`.

use std::fmt::Write as _;

use clap::{Args, Subcommand};
use controlz_core::ac::setup::{self, DeviceInfo};

use crate::secrets::Error;

#[derive(Debug, Subcommand)]
pub enum AcCmd {
    /// Talk to the AC's Wi-Fi module while it is in setup (hotspot) mode
    Setup(SetupArgs),
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
    #[arg(long, hide = true)]
    init: bool,
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
    }
}

fn run_setup(args: &SetupArgs) -> Result<(), Error> {
    if !args.info_only {
        return Err("full setup arrives in T1.9; use --info-only".into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let info = runtime.block_on(setup::read_device_info(&args.host, args.port, args.init))?;
    print!("{}", render(&info, args.show_ids));
    Ok(())
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
    fn full_setup_is_refused() {
        let args = SetupArgs {
            info_only: false,
            host: "controlz.test".to_owned(),
            port: setup::SETUP_PORT,
            show_ids: false,
            init: false,
        };
        let err = run_setup(&args).unwrap_err();
        assert!(err.to_string().contains("T1.9"), "{err}");
    }
}
