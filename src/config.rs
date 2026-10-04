use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const CONFIG_RELATIVE_PATH: &str = ".config/eventkit-bridge/config.toml";
const EKCTL_FILE_NAME: &str = "ekctl";

/// An EventKit calendar identifier as `ekctl` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CalendarId(String);

impl CalendarId {
    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CalendarId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The validated bridge configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The literal socket address the HTTP server binds to.
    pub listen: SocketAddr,
    /// The calendars reads may touch, as listed in the config file.
    pub read_calendars: Vec<CalendarId>,
    /// The only calendar writes may touch; `None` refuses every write.
    pub write_calendar: Option<CalendarId>,
    /// An explicit `ekctl` path; `None` means `ekctl` next to the running executable.
    pub ekctl: Option<PathBuf>,
}

/// Why a config file was rejected; every rule violation names its key.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `$HOME` is unset or empty, so the default config path is unknown.
    #[error("HOME is not set, cannot locate the config file")]
    NoHome,
    /// The config file could not be read.
    #[error("cannot read the config file: {0}")]
    Read(#[source] io::Error),
    /// The config file is not valid TOML or has an unknown key or a wrong type.
    #[error("invalid config: {0}")]
    Parse(#[source] toml::de::Error),
    /// `listen` is absent.
    #[error("`listen` is required")]
    MissingListen,
    /// `listen` is not a literal `IP:port`.
    #[error("`listen` must be a literal IP:port, got {0:?}")]
    ListenNotLiteral(String),
    /// `listen` names an address that would bind every interface.
    #[error("`listen` must not be an unspecified address, got {0}")]
    ListenUnspecified(SocketAddr),
    /// A calendar id in `read_calendars` or `write_calendar` is unusable.
    #[error("`{key}` contains an invalid calendar id {value:?}: {reason}")]
    InvalidCalendarId {
        /// The config key holding the id.
        key: &'static str,
        /// The id as written.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// `ekctl` is set to an empty path.
    #[error("`ekctl` must not be empty")]
    EmptyEkctl,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: Option<String>,
    #[serde(default)]
    read_calendars: Vec<String>,
    write_calendar: Option<String>,
    ekctl: Option<PathBuf>,
}

impl Config {
    /// Returns `$HOME/.config/eventkit-bridge/config.toml`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        path_under_home(env::var_os("HOME"))
    }

    /// Reads and validates the config file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::from_toml(&text)
    }

    /// Validates a config given as TOML text.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let RawConfig {
            listen,
            read_calendars,
            write_calendar,
            ekctl,
        } = toml::from_str(text).map_err(ConfigError::Parse)?;

        let Some(listen) = listen else {
            return Err(ConfigError::MissingListen);
        };
        let listen = parse_listen(&listen)?;

        let mut read = Vec::new();
        for id in read_calendars {
            read.push(parse_calendar_id("read_calendars", id)?);
        }

        let write = match write_calendar {
            Some(id) => Some(parse_calendar_id("write_calendar", id)?),
            None => None,
        };

        if let Some(path) = &ekctl
            && path.as_os_str().is_empty()
        {
            return Err(ConfigError::EmptyEkctl);
        }

        Ok(Self {
            listen,
            read_calendars: read,
            write_calendar: write,
            ekctl,
        })
    }

    /// Every calendar reads may touch: `read_calendars` plus the write calendar, without duplicates.
    pub fn readable_calendars(&self) -> Vec<CalendarId> {
        let mut readable = Vec::new();
        for id in self
            .read_calendars
            .iter()
            .chain(self.write_calendar.as_ref())
        {
            if !readable.contains(id) {
                readable.push(id.clone());
            }
        }
        readable
    }

    /// The `ekctl` to run: the configured path, or `ekctl` beside `executable`.
    pub fn ekctl_path(&self, executable: &Path) -> PathBuf {
        if let Some(path) = &self.ekctl {
            return path.clone();
        }
        let Some(dir) = executable.parent() else {
            return PathBuf::from(EKCTL_FILE_NAME);
        };
        dir.join(EKCTL_FILE_NAME)
    }
}

fn path_under_home(home: Option<OsString>) -> Result<PathBuf, ConfigError> {
    let Some(home) = home else {
        return Err(ConfigError::NoHome);
    };
    if home.is_empty() {
        return Err(ConfigError::NoHome);
    }
    Ok(PathBuf::from(home).join(CONFIG_RELATIVE_PATH))
}

fn parse_listen(value: &str) -> Result<SocketAddr, ConfigError> {
    let Ok(addr) = value.parse::<SocketAddr>() else {
        return Err(ConfigError::ListenNotLiteral(value.to_owned()));
    };
    if addr.ip().is_unspecified() {
        return Err(ConfigError::ListenUnspecified(addr));
    }
    Ok(addr)
}

fn parse_calendar_id(key: &'static str, value: String) -> Result<CalendarId, ConfigError> {
    let reason = if value.trim().is_empty() {
        Some("empty")
    } else if value.contains(',') {
        Some("contains a comma")
    } else if has_control_character(&value) {
        Some("contains a control character")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ConfigError::InvalidCalendarId { key, value, reason }),
        None => Ok(CalendarId(value)),
    }
}

fn has_control_character(value: &str) -> bool {
    for c in value.chars() {
        if c.is_control() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";

    fn id(value: &str) -> CalendarId {
        CalendarId(value.to_owned())
    }

    #[test]
    fn valid_config() {
        let config = Config::from_toml(&format!(
            r#"
            listen = "100.108.208.81:8790"
            read_calendars = ["{READ_ID}"]
            write_calendar = "{WRITE_ID}"
            ekctl = "/opt/ekctl"
            "#
        ))
        .unwrap();

        assert_eq!(
            config,
            Config {
                listen: "100.108.208.81:8790".parse().unwrap(),
                read_calendars: vec![id(READ_ID)],
                write_calendar: Some(id(WRITE_ID)),
                ekctl: Some(PathBuf::from("/opt/ekctl")),
            }
        );
    }

    #[test]
    fn ipv6_listen() {
        let config = Config::from_toml(r#"listen = "[fd7a:115c:a1e0::1]:8790""#).unwrap();
        assert_eq!(config.listen, "[fd7a:115c:a1e0::1]:8790".parse().unwrap());
    }

    #[test]
    fn minimal_config_is_first_run() {
        let config = Config::from_toml(r#"listen = "127.0.0.1:8790""#).unwrap();
        assert!(config.read_calendars.is_empty());
        assert_eq!(config.write_calendar, None);
        assert_eq!(config.ekctl, None);
        assert!(config.readable_calendars().is_empty());
    }

    #[test]
    fn missing_listen() {
        let err = Config::from_toml(&format!(r#"read_calendars = ["{READ_ID}"]"#)).unwrap_err();
        assert!(err.to_string().contains("`listen`"));
        let ConfigError::MissingListen = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[test]
    fn unspecified_ipv4_listen() {
        let err = Config::from_toml(r#"listen = "0.0.0.0:8790""#).unwrap_err();
        let ConfigError::ListenUnspecified(addr) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(addr, "0.0.0.0:8790".parse().unwrap());
    }

    #[test]
    fn unspecified_ipv6_listen() {
        let err = Config::from_toml(r#"listen = "[::]:8790""#).unwrap_err();
        let ConfigError::ListenUnspecified(_) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[test]
    fn hostname_listen() {
        let err = Config::from_toml(r#"listen = "localhost:8790""#).unwrap_err();
        let ConfigError::ListenNotLiteral(value) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(value, "localhost:8790");
        assert!(err.to_string().contains("`listen`"));
    }

    #[test]
    fn listen_without_port() {
        let err = Config::from_toml(r#"listen = "100.108.208.81""#).unwrap_err();
        let ConfigError::ListenNotLiteral(_) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[test]
    fn malformed_toml() {
        let err = Config::from_toml(r#"listen = "127.0.0.1:8790"#).unwrap_err();
        let ConfigError::Parse(_) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[test]
    fn wrong_type_names_key() {
        let err = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            read_calendars = "not-a-list"
            "#,
        )
        .unwrap_err();
        let ConfigError::Parse(_) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(err.to_string().contains("read_calendars"), "{err}");
    }

    #[test]
    fn unknown_key_names_key() {
        let err = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            write_calendars = ["x"]
            "#,
        )
        .unwrap_err();
        let ConfigError::Parse(_) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(err.to_string().contains("write_calendars"), "{err}");
    }

    #[test]
    fn empty_read_calendar_id() {
        let err = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            read_calendars = [" "]
            "#,
        )
        .unwrap_err();
        let ConfigError::InvalidCalendarId { key, .. } = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(key, "read_calendars");
    }

    #[test]
    fn write_calendar_id_with_comma() {
        let err = Config::from_toml(&format!(
            r#"
            listen = "127.0.0.1:8790"
            write_calendar = "{READ_ID},{WRITE_ID}"
            "#
        ))
        .unwrap_err();
        let ConfigError::InvalidCalendarId { key, reason, .. } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(*key, "write_calendar");
        assert_eq!(*reason, "contains a comma");
        assert!(err.to_string().contains("`write_calendar`"));
    }

    #[test]
    fn calendar_id_with_control_character() {
        let err = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            read_calendars = ["abc\u0007"]
            "#,
        )
        .unwrap_err();
        let ConfigError::InvalidCalendarId { reason, .. } = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(reason, "contains a control character");
    }

    #[test]
    fn empty_ekctl() {
        let err = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            ekctl = ""
            "#,
        )
        .unwrap_err();
        let ConfigError::EmptyEkctl = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[test]
    fn write_calendar_implied_readable() {
        let config = Config::from_toml(&format!(
            r#"
            listen = "127.0.0.1:8790"
            read_calendars = ["{READ_ID}"]
            write_calendar = "{WRITE_ID}"
            "#
        ))
        .unwrap();
        assert_eq!(config.readable_calendars(), vec![id(READ_ID), id(WRITE_ID)]);
    }

    #[test]
    fn write_calendar_readable_without_read_list() {
        let config = Config::from_toml(&format!(
            r#"
            listen = "127.0.0.1:8790"
            write_calendar = "{WRITE_ID}"
            "#
        ))
        .unwrap();
        assert!(config.read_calendars.is_empty());
        assert_eq!(config.readable_calendars(), vec![id(WRITE_ID)]);
    }

    #[test]
    fn write_calendar_listed_as_read_is_not_duplicated() {
        let config = Config::from_toml(&format!(
            r#"
            listen = "127.0.0.1:8790"
            read_calendars = ["{WRITE_ID}", "{READ_ID}", "{READ_ID}"]
            write_calendar = "{WRITE_ID}"
            "#
        ))
        .unwrap();
        assert_eq!(config.readable_calendars(), vec![id(WRITE_ID), id(READ_ID)]);
    }

    #[test]
    fn absent_write_calendar() {
        let config = Config::from_toml(&format!(
            r#"
            listen = "127.0.0.1:8790"
            read_calendars = ["{READ_ID}"]
            "#
        ))
        .unwrap();
        assert_eq!(config.write_calendar, None);
        assert_eq!(config.readable_calendars(), vec![id(READ_ID)]);
    }

    #[test]
    fn ekctl_defaults_next_to_executable() {
        let config = Config::from_toml(r#"listen = "127.0.0.1:8790""#).unwrap();
        let executable =
            Path::new("/Applications/EventKitBridge.app/Contents/MacOS/eventkit-bridge");
        assert_eq!(
            config.ekctl_path(executable),
            PathBuf::from("/Applications/EventKitBridge.app/Contents/MacOS/ekctl")
        );
    }

    #[test]
    fn ekctl_override() {
        let config = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            ekctl = "/usr/local/bin/ekctl"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.ekctl_path(Path::new("/somewhere/eventkit-bridge")),
            PathBuf::from("/usr/local/bin/ekctl")
        );
    }

    #[test]
    fn default_path_under_home() {
        assert_eq!(
            path_under_home(Some(OsString::from("/Users/someone"))).unwrap(),
            PathBuf::from("/Users/someone/.config/eventkit-bridge/config.toml")
        );
    }

    #[test]
    fn default_path_without_home() {
        let ConfigError::NoHome = path_under_home(None).unwrap_err() else {
            panic!("expected NoHome");
        };
        let ConfigError::NoHome = path_under_home(Some(OsString::new())).unwrap_err() else {
            panic!("expected NoHome");
        };
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            format!("listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\"]\n"),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.read_calendars, vec![id(READ_ID)]);
    }

    #[test]
    fn load_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = Config::load(&dir.path().join("absent.toml")).unwrap_err();
        let ConfigError::Read(source) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(source.kind(), io::ErrorKind::NotFound);
    }
}
