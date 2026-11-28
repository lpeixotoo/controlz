//! `controlz config ...`: clap glue over `controlz_core::Config` and the Keychain.

use std::io::{BufRead, IsTerminal};

use clap::Subcommand;
use controlz_core::{Config, Secret};

use crate::secrets::{self, Error, SERVICE};

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Set a config key (tv.host, tv.mac, ac.uuid, ac.bind, ac.host)
    Set { key: String, value: String },
    /// Print a config key
    Get { key: String },
    /// Print the whole config file
    Show,
    /// Print the config file path
    Path,
    /// Manage secrets stored in the macOS Keychain
    #[command(subcommand)]
    Secret(SecretCmd),
}

#[derive(Debug, Subcommand)]
pub enum SecretCmd {
    /// Store a secret; the value is read from piped stdin, never from arguments
    Set { name: String },
    /// Print whether a secret is set (never its value)
    Status { name: String },
    /// Delete a secret
    Delete { name: String },
}

pub fn run(cmd: ConfigCmd) -> Result<(), Error> {
    let path = Config::default_path()?;
    match cmd {
        ConfigCmd::Set { key, value } => {
            let mut cfg = Config::load_from(&path)?;
            cfg.set(&key, &value)?;
            cfg.save_to(&path)?;
            tracing::info!(key, path = %path.display(), "config saved");
        }
        ConfigCmd::Get { key } => match Config::load_from(&path)?.get(&key)? {
            Some(value) => println!("{value}"),
            None => return Err(format!("{key} is not set").into()),
        },
        ConfigCmd::Show => print!("{}", Config::load_from(&path)?.to_toml()?),
        ConfigCmd::Path => println!("{}", path.display()),
        ConfigCmd::Secret(cmd) => run_secret(cmd)?,
    }
    Ok(())
}

fn run_secret(cmd: SecretCmd) -> Result<(), Error> {
    match cmd {
        SecretCmd::Set { name } => {
            secrets::check_name(&name)?;
            let value = read_secret_from_stdin(&name)?;
            secrets::set(SERVICE, &name, &value)?;
            println!("{name}: saved to Keychain");
        }
        SecretCmd::Status { name } => {
            secrets::check_name(&name)?;
            let state = if secrets::get(SERVICE, &name)?.is_some() {
                "set"
            } else {
                "not set"
            };
            println!("{name}: {state}");
        }
        SecretCmd::Delete { name } => {
            secrets::check_name(&name)?;
            if secrets::delete(SERVICE, &name)? {
                println!("{name}: deleted");
            } else {
                println!("{name}: not set");
            }
        }
    }
    Ok(())
}

fn read_secret_from_stdin(name: &str) -> Result<Secret, Error> {
    let stdin = std::io::stdin();
    // Typing or pasting at a terminal echoes the value into scrollback.
    if stdin.is_terminal() {
        return Err(format!(
            "refusing to read {name} from the terminal (it would echo); \
             pipe it in: pbpaste | controlz config secret set {name}"
        )
        .into());
    }
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let value = line.trim_end_matches(['\n', '\r']);
    if value.is_empty() {
        return Err(format!("no value for {name} on stdin").into());
    }
    Ok(Secret::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use controlz_core::CONFIG_KEYS;

    #[test]
    fn set_help_names_every_config_key() {
        let cmd = ConfigCmd::augment_subcommands(clap::Command::new("config"));
        let about = cmd
            .find_subcommand("set")
            .and_then(clap::Command::get_about)
            .map(ToString::to_string)
            .unwrap();
        for key in CONFIG_KEYS {
            assert!(about.contains(key), "{key} missing from {about:?}");
        }
    }
}
