#![forbid(unsafe_code)]

mod config;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use argh::FromArgs;

use crate::config::Config;

/// Exposes the Mac's calendars over HTTP through ekctl.
#[derive(FromArgs, Debug, PartialEq, Eq)]
struct Cli {
    /// validate the config file and exit
    #[argh(switch)]
    check_config: bool,
    /// print the version and exit
    #[argh(switch)]
    version: bool,
    #[argh(subcommand)]
    command: Option<Command>,
}

#[derive(FromArgs, Debug, PartialEq, Eq)]
#[argh(subcommand)]
enum Command {
    Install(InstallArgs),
    Uninstall(UninstallArgs),
}

/// Write and load the LaunchAgent.
#[derive(FromArgs, Debug, PartialEq, Eq)]
#[argh(subcommand, name = "install")]
struct InstallArgs {}

/// Unload and remove the LaunchAgent, keeping the config.
#[derive(FromArgs, Debug, PartialEq, Eq)]
#[argh(subcommand, name = "uninstall")]
struct UninstallArgs {}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Version,
    CheckConfig,
    Install,
    Uninstall,
    Daemon,
}

impl Cli {
    fn action(&self) -> Result<Action, &'static str> {
        let Cli {
            check_config,
            version,
            command,
        } = self;
        if *version {
            return Ok(Action::Version);
        }
        match (check_config, command) {
            (true, Some(_)) => Err("--check-config cannot be combined with a subcommand"),
            (true, None) => Ok(Action::CheckConfig),
            (false, Some(Command::Install(InstallArgs {}))) => Ok(Action::Install),
            (false, Some(Command::Uninstall(UninstallArgs {}))) => Ok(Action::Uninstall),
            (false, None) => Ok(Action::Daemon),
        }
    }
}

fn main() -> ExitCode {
    let cli: Cli = argh::from_env();
    let action = match cli.action() {
        Ok(action) => action,
        Err(message) => {
            eprintln!("eventkit-bridge: {message}");
            return ExitCode::FAILURE;
        }
    };
    let result = match action {
        Action::Version => {
            println!("eventkit-bridge {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Action::CheckConfig => check_config(),
        Action::Install => Err("install is not implemented yet".to_owned()),
        Action::Uninstall => Err("uninstall is not implemented yet".to_owned()),
        Action::Daemon => run_daemon(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("eventkit-bridge: {message}");
            ExitCode::FAILURE
        }
    }
}

fn load_config() -> Result<(PathBuf, Config), String> {
    let path = Config::default_path().map_err(|err| err.to_string())?;
    let config = Config::load(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok((path, config))
}

fn check_config() -> Result<(), String> {
    let (path, config) = load_config()?;
    print!("{}", describe(&path, &config));
    Ok(())
}

fn run_daemon() -> Result<(), String> {
    load_config()?;
    Ok(())
}

fn describe(path: &Path, config: &Config) -> String {
    let Config {
        listen,
        read_calendars,
        write_calendar,
        ekctl: _,
    } = config;
    let ekctl = match env::current_exe() {
        Ok(executable) => config.ekctl_path(&executable).display().to_string(),
        Err(err) => format!("unknown ({err})"),
    };
    let mut out = format!("config ok: {}\nlisten: {listen}\n", path.display());
    if read_calendars.is_empty() {
        out.push_str("read_calendars: none (unconfigured)\n");
    }
    for id in config.readable_calendars() {
        out.push_str(&format!("readable: {id}\n"));
    }
    match write_calendar {
        Some(id) => out.push_str(&format!("write_calendar: {id}\n")),
        None => out.push_str("write_calendar: none (writes refused)\n"),
    }
    out.push_str(&format!("ekctl: {ekctl}\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, argh::EarlyExit> {
        Cli::from_args(&["eventkit-bridge"], args)
    }

    #[test]
    fn no_arguments_runs_daemon() {
        assert_eq!(parse(&[]).unwrap().action(), Ok(Action::Daemon));
    }

    #[test]
    fn version_switch() {
        assert_eq!(parse(&["--version"]).unwrap().action(), Ok(Action::Version));
    }

    #[test]
    fn check_config_switch() {
        assert_eq!(
            parse(&["--check-config"]).unwrap().action(),
            Ok(Action::CheckConfig)
        );
    }

    #[test]
    fn install_subcommand() {
        assert_eq!(parse(&["install"]).unwrap().action(), Ok(Action::Install));
    }

    #[test]
    fn uninstall_subcommand() {
        assert_eq!(
            parse(&["uninstall"]).unwrap().action(),
            Ok(Action::Uninstall)
        );
    }

    #[test]
    fn check_config_with_subcommand_is_rejected() {
        assert!(
            parse(&["--check-config", "install"])
                .unwrap()
                .action()
                .is_err()
        );
    }

    #[test]
    fn unknown_argument_is_rejected() {
        assert!(parse(&["--listen"]).is_err());
        assert!(parse(&["serve"]).is_err());
    }

    #[test]
    fn describe_unconfigured() {
        let config = Config::from_toml(r#"listen = "127.0.0.1:8790""#).unwrap();
        let text = describe(Path::new("/tmp/config.toml"), &config);
        assert!(text.starts_with("config ok: /tmp/config.toml\nlisten: 127.0.0.1:8790\n"));
        assert!(text.contains("read_calendars: none (unconfigured)\n"));
        assert!(text.contains("write_calendar: none (writes refused)\n"));
    }

    #[test]
    fn describe_configured() {
        let config = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            read_calendars = ["READ"]
            write_calendar = "WRITE"
            ekctl = "/opt/ekctl"
            "#,
        )
        .unwrap();
        let text = describe(Path::new("/tmp/config.toml"), &config);
        assert!(text.contains("readable: READ\nreadable: WRITE\n"));
        assert!(text.contains("write_calendar: WRITE\n"));
        assert!(text.contains("ekctl: /opt/ekctl\n"));
        assert!(!text.contains("unconfigured"));
    }
}
