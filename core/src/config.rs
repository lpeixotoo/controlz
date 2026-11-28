//! Non-secret device configuration (D10), shared by the CLI and the app.
//!
//! Stored as TOML at `~/Library/Application Support/Controlz/config.toml`.
//! Secrets (the TV client-key) never go here; they live in the Keychain.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ControlError, Result};
use crate::wol::parse_mac;

/// Keys accepted by [`Config::set`] and [`Config::get`].
pub const CONFIG_KEYS: &[&str] = &["tv.host", "tv.mac", "ac.uuid", "ac.bind", "ac.host"];

/// Whole configuration file. Each device has an optional section.
///
/// Unknown keys (for example written by a newer app or CLI) are kept in
/// `extra` and written back on save, so an older binary neither fails to load
/// the file nor drops what it does not understand. `Eq` is absent because
/// arbitrary TOML values may hold floats.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tv: Option<TvConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ac: Option<AcConfig>,
    #[serde(flatten, default, skip_serializing_if = "toml::Table::is_empty")]
    extra: toml::Table,
}

/// `[tv]` section: LG webOS TV.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TvConfig {
    /// IP address or host name of the TV.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// MAC address for Wake-on-LAN, normalised to lowercase colon-separated hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "toml::Table::is_empty")]
    extra: toml::Table,
}

/// `[ac]` section: air conditioner on the local cloud route (D20), written by
/// `controlz ac setup`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AcConfig {
    /// The Wi-Fi module's `ThinQ` device id (from `getDeviceInfo`). Private:
    /// it stays in this local file, never in logs or the repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// This Mac's LAN address the local cloud and broker listen on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// The host the module is told to use (an IP or a DNS name); also the
    /// server certificate's SAN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "toml::Table::is_empty")]
    extra: toml::Table,
}

impl Config {
    /// `$HOME/Library/Application Support/Controlz/config.toml`.
    ///
    /// # Errors
    /// [`ControlError::Config`] if the home directory cannot be found.
    pub fn default_path() -> Result<PathBuf> {
        let home = std::env::home_dir()
            .ok_or_else(|| ControlError::config("cannot find the home directory"))?;
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("Controlz")
            .join("config.toml"))
    }

    /// Reads the file at `path`. A missing file gives the default (empty) config.
    ///
    /// # Errors
    /// [`ControlError::Config`] if the file cannot be read or is not valid TOML.
    pub fn load_from(path: &Path) -> Result<Self> {
        tracing::debug!(path = %path.display(), "loading config");
        match fs::read_to_string(path) {
            Ok(text) => {
                let cfg: Self = toml::from_str(&text)
                    .map_err(|e| ControlError::config(format!("{}: {e}", path.display())))?;
                cfg.warn_unknown_keys(path);
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!("config file missing, using defaults");
                Ok(Self::default())
            }
            Err(e) => Err(ControlError::config(format!("{}: {e}", path.display()))),
        }
    }

    /// Logs the names (never the values) of keys this binary does not know.
    fn warn_unknown_keys(&self, path: &Path) {
        let mut names: Vec<String> = self.extra.keys().cloned().collect();
        if let Some(tv) = &self.tv {
            names.extend(tv.extra.keys().map(|k| format!("tv.{k}")));
        }
        if let Some(ac) = &self.ac {
            names.extend(ac.extra.keys().map(|k| format!("ac.{k}")));
        }
        if !names.is_empty() {
            tracing::warn!(
                path = %path.display(),
                keys = %names.join(", "),
                "unknown config keys kept as-is"
            );
        }
    }

    /// Writes the config to `path` atomically (temp file + rename), creating
    /// the parent directory if needed. The temp name includes the process id
    /// so the CLI and the app saving at once do not clobber each other's file.
    ///
    /// # Errors
    /// [`ControlError::Config`] if the directory, temp file or rename fails.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        tracing::debug!(path = %path.display(), "saving config");
        let io_err = |e: std::io::Error| ControlError::config(format!("{}: {e}", path.display()));
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io_err)?;
        }
        let text = self.to_toml()?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let written = fs::write(&tmp, text).and_then(|()| fs::rename(&tmp, path));
        if let Err(e) = written {
            // Best effort: the original error matters more than cleanup failing.
            let _ = fs::remove_file(&tmp);
            return Err(io_err(e));
        }
        Ok(())
    }

    /// The config as TOML text.
    ///
    /// # Errors
    /// [`ControlError::Config`] if serialization fails.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).map_err(ControlError::config)
    }

    /// Sets a dotted key (see [`CONFIG_KEYS`]). Values are validated.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] for an unknown key or a bad value.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let value = value.trim();
        if value.is_empty() {
            return Err(ControlError::invalid(format!("empty value for {key}")));
        }
        match key {
            "tv.host" => {
                check_host(value)?;
                self.tv.get_or_insert_default().host = Some(value.to_owned());
            }
            "ac.uuid" => {
                let bad = value.len() > MAX_UUID_LEN
                    || value.contains(['/', '?', '#', '+', '\0'])
                    || value.chars().any(char::is_whitespace);
                if bad {
                    // The value is private: do not echo it.
                    return Err(ControlError::invalid("bad ac.uuid"));
                }
                self.ac.get_or_insert_default().uuid = Some(value.to_owned());
            }
            "ac.bind" => {
                let ip: IpAddr = value.parse().map_err(|_| {
                    ControlError::invalid(format!("ac.bind must be an IP address: {value}"))
                })?;
                if ip.is_unspecified() {
                    return Err(ControlError::invalid(
                        "ac.bind must be one interface address, not the unspecified address",
                    ));
                }
                self.ac.get_or_insert_default().bind = Some(ip.to_string());
            }
            "ac.host" => {
                check_host(value)?;
                self.ac.get_or_insert_default().host = Some(value.to_owned());
            }
            "tv.mac" => {
                let bytes = parse_mac(value)?;
                let mac = bytes
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(":");
                self.tv.get_or_insert_default().mac = Some(mac);
            }
            _ => return Err(unknown_key(key)),
        }
        Ok(())
    }

    /// Reads a dotted key (see [`CONFIG_KEYS`]); `None` when it is not set.
    ///
    /// # Errors
    /// [`ControlError::Invalid`] for an unknown key.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        let tv = self.tv.as_ref();
        let ac = self.ac.as_ref();
        match key {
            "tv.host" => Ok(tv.and_then(|t| t.host.clone())),
            "tv.mac" => Ok(tv.and_then(|t| t.mac.clone())),
            "ac.uuid" => Ok(ac.and_then(|a| a.uuid.clone())),
            "ac.bind" => Ok(ac.and_then(|a| a.bind.clone())),
            "ac.host" => Ok(ac.and_then(|a| a.host.clone())),
            _ => Err(unknown_key(key)),
        }
    }
}

/// Longest `ac.uuid` accepted (the MQTT broker's limit).
const MAX_UUID_LEN: usize = 128;

fn check_host(value: &str) -> Result<()> {
    if value.chars().any(char::is_whitespace) {
        return Err(ControlError::invalid(format!("bad host: {value}")));
    }
    Ok(())
}

fn unknown_key(key: &str) -> ControlError {
    ControlError::invalid(format!(
        "unknown config key {key} (known: {})",
        CONFIG_KEYS.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "controlz-test-{name}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn default_path_is_application_support() {
        let path = Config::default_path().unwrap();
        assert!(
            path.ends_with("Library/Application Support/Controlz/config.toml"),
            "{}",
            path.display()
        );
    }

    #[test]
    fn missing_file_loads_default() {
        let dir = TempDir::new("missing");
        let cfg = Config::load_from(&dir.0.join("nope.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn set_save_load_round_trip() {
        let dir = TempDir::new("roundtrip");
        let path = dir.0.join("sub").join("config.toml");

        let mut cfg = Config::default();
        cfg.set("tv.host", "192.0.2.1").unwrap(); // privacy:allow (TEST-NET-1)
        cfg.set("tv.mac", "02-00-00-00-00-0A").unwrap(); // privacy:allow (fake MAC)
        cfg.save_to(&path).unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, cfg);
        assert_eq!(loaded.get("tv.host").unwrap().as_deref(), Some("192.0.2.1")); // privacy:allow
        assert_eq!(
            loaded.get("tv.mac").unwrap().as_deref(),
            Some("02:00:00:00:00:0a") // privacy:allow (fake MAC)
        );
        assert_no_tmp_files(&dir.0.join("sub"));

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[tv]"), "{text}");
    }

    #[test]
    fn unset_key_is_none() {
        let cfg = Config::default();
        assert_eq!(cfg.get("tv.host").unwrap(), None);
        assert_eq!(cfg.get("tv.mac").unwrap(), None);
    }

    #[test]
    fn rejects_unknown_key_and_bad_values() {
        let mut cfg = Config::default();
        assert!(cfg.set("tv.port", "3000").is_err());
        assert!(cfg.get("tv.client-key").is_err());
        assert!(cfg.set("tv.mac", "not-a-mac").is_err());
        assert!(cfg.set("tv.host", "  ").is_err());
        assert!(cfg.set("tv.host", "a b").is_err());
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn ac_keys_round_trip_and_validate() {
        let dir = TempDir::new("ac");
        let path = dir.0.join("config.toml");
        let mut cfg = Config::default();
        assert_eq!(cfg.get("ac.uuid").unwrap(), None);
        cfg.set("ac.uuid", "00000000-fake-0000-0000-000000000000")
            .unwrap();
        cfg.set("ac.bind", "192.0.2.5").unwrap(); // privacy:allow (TEST-NET-1)
        cfg.set("ac.host", "192-0-2-5.sslip.io").unwrap();
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, cfg);
        assert_eq!(
            loaded.get("ac.uuid").unwrap().as_deref(),
            Some("00000000-fake-0000-0000-000000000000")
        );
        assert_eq!(loaded.get("ac.bind").unwrap().as_deref(), Some("192.0.2.5")); // privacy:allow
        assert_eq!(
            loaded.get("ac.host").unwrap().as_deref(),
            Some("192-0-2-5.sslip.io")
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[ac]"), "{text}");

        // IPv6 is normalised; bad values change nothing.
        cfg.set("ac.bind", "2001:DB8:0::1").unwrap();
        assert_eq!(cfg.get("ac.bind").unwrap().as_deref(), Some("2001:db8::1"));
        let before = cfg.clone();
        for (key, value) in [
            ("ac.bind", "0.0.0.0"),
            ("ac.bind", "::"),
            ("ac.bind", "mac.local"),
            ("ac.uuid", "a/b"),
            ("ac.uuid", "a b"),
            ("ac.host", "a b"),
        ] {
            assert!(cfg.set(key, value).is_err(), "{key}={value}");
        }
        let long = "u".repeat(MAX_UUID_LEN + 1);
        let err = cfg.set("ac.uuid", &long).unwrap_err();
        assert!(!err.to_string().contains(&long), "uuid echoed: {err}");
        assert_eq!(cfg, before);
    }

    fn assert_no_tmp_files(dir: &Path) {
        let leftovers: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| Path::new(n).extension().is_some_and(|e| e == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn keeps_unknown_fields_round_trip() {
        let dir = TempDir::new("unknown");
        let path = dir.0.join("config.toml");
        fs::write(
            &path,
            "[tv]\nhost = \"192.0.2.1\"\nport = 3001\n\n[future]\nflag = true\nname = \"x\"\n", // privacy:allow (TEST-NET-1)
        )
        .unwrap();

        let mut cfg = Config::load_from(&path).unwrap();
        cfg.set("tv.host", "192.0.2.2").unwrap(); // privacy:allow (TEST-NET-1)
        cfg.save_to(&path).unwrap();

        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(reloaded, cfg);
        assert_eq!(
            reloaded.get("tv.host").unwrap().as_deref(),
            Some("192.0.2.2")
        ); // privacy:allow
        let tv = reloaded.tv.as_ref().unwrap();
        assert_eq!(tv.extra.get("port"), Some(&toml::Value::Integer(3001)));
        assert_eq!(tv.extra.len(), 1);
        let future = reloaded
            .extra
            .get("future")
            .and_then(toml::Value::as_table)
            .unwrap();
        assert_eq!(future.get("flag"), Some(&toml::Value::Boolean(true)));
        assert_eq!(
            future.get("name"),
            Some(&toml::Value::String("x".to_owned()))
        );
        assert_eq!(future.len(), 2);
        assert_eq!(reloaded.extra.len(), 1);
        assert_no_tmp_files(&dir.0);
    }

    /// Pins the on-disk TOML layout. The MAC is left out on purpose: any MAC
    /// in the `.snap` file would trip the privacy pre-commit hook.
    #[test]
    fn config_toml_snapshot() {
        let mut cfg = Config::default();
        cfg.set("tv.host", "192.0.2.1").unwrap(); // privacy:allow (TEST-NET-1)
        insta::assert_snapshot!(cfg.to_toml().unwrap());
    }
}
