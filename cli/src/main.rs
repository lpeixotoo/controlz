//! `controlz` command-line tool: control LG devices and manage their config.

mod ac;
mod config;
mod secrets;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};
use tracing::level_filters::LevelFilter;

#[derive(Debug, Parser)]
#[command(
    name = "controlz",
    version,
    about = "Control LG devices on the local network"
)]
struct Cli {
    /// More log output on stderr (-v info, -vv debug, -vvv trace)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Air conditioner commands
    #[command(subcommand)]
    Ac(ac::AcCmd),
    /// TV commands (not implemented yet)
    Tv,
    /// Read and write config and Keychain secrets
    #[command(subcommand)]
    Config(config::ConfigCmd),
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let result = match cli.command {
        Command::Ac(cmd) => ac::run(cmd),
        Command::Tv => Err("tv commands are not implemented yet".into()),
        Command::Config(cmd) => config::run(cmd),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_is_valid_and_lists_groups() {
        Cli::command().debug_assert();
        let names: Vec<_> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        for group in ["ac", "tv", "config"] {
            assert!(names.iter().any(|n| n == group), "missing {group}");
        }
    }

    #[test]
    fn ac_setup_requires_host() {
        let parse =
            |args: &[&str]| Cli::try_parse_from([&["controlz", "ac", "setup"], args].concat());
        assert!(parse(&["--info-only"]).is_err());
        assert!(parse(&["--info-only", "--host", "controlz.test", "--show-ids"]).is_ok());
    }
}
