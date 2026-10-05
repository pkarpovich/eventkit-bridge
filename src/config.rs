use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CONFIG_RELATIVE_PATH: &str = ".config/eventkit-bridge/config.toml";
const EKCTL_FILE_NAME: &str = "ekctl";
const REMINDCTL_FILE_NAME: &str = "remindctl";
const NAME_MAX_LEN: usize = 40;
const UUID_GROUP_LENGTHS: [usize; 5] = [8, 4, 4, 4, 12];
const MAIL_RELATIVE_PATH: &str = "Library/Mail";

/// Where the Envelope Index lives under a `V<n>` mail root.
pub const MAIL_INDEX_RELATIVE_PATH: &str = "MailData/Envelope Index";

/// The mailboxes left out when `[mail]` has no `exclude_mailboxes`.
pub const DEFAULT_EXCLUDED_MAILBOXES: [&str; 8] = [
    "Trash",
    "Deleted Items",
    "Deleted Messages",
    "Junk",
    "Junk Email",
    "Spam",
    "[Gmail]/Trash",
    "[Gmail]/Spam",
];

/// The radius in meters a place gets when the config gives none.
pub const DEFAULT_RADIUS: u32 = 100;
/// The smallest radius in meters a place may have.
pub const MIN_RADIUS: u32 = 50;
/// The largest radius in meters a place may have.
pub const MAX_RADIUS: u32 = 2000;

/// An EventKit calendar identifier as `ekctl` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct CalendarId(String);

impl CalendarId {
    /// Accepts an id that can be passed to `ekctl`: not blank, no comma, no control character.
    pub fn parse(value: String) -> Result<Self, &'static str> {
        if value.trim().is_empty() {
            return Err("empty");
        }
        if value.contains(',') {
            return Err("contains a comma");
        }
        if has_control_character(&value) {
            return Err("contains a control character");
        }
        Ok(Self(value))
    }

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

/// A reminder list identifier, always a full UUID so `remindctl` never reads it as a row index.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ListId(String);

impl ListId {
    /// Accepts a full UUID such as `4F7D9489-A78F-4369-A951-213207DCFEE3`, in uppercase as
    /// `remindctl` reports it.
    pub fn parse(value: String) -> Result<Self, &'static str> {
        if !is_full_uuid(&value) {
            return Err("must be a full UUID");
        }
        Ok(Self(value.to_ascii_uppercase()))
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ListId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ListId> for String {
    fn from(id: ListId) -> Self {
        id.0
    }
}

impl fmt::Display for ListId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The name a client uses to pick a configured place for a location trigger.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PlaceName(String);

impl PlaceName {
    /// Accepts 1 to 40 lowercase letters, digits and `-`, starting with a letter or digit.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        check_name(value)?;
        Ok(Self(value.to_owned()))
    }

    /// Returns the name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A street address that `remindctl` geocodes; its `Debug` output hides the text so it never reaches a log.
#[derive(Clone, PartialEq, Eq)]
pub struct Address(String);

impl Address {
    /// Returns the address as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Address(<redacted>)")
    }
}

/// A place a new reminder's location trigger may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The name clients use.
    pub name: PlaceName,
    /// The street address passed to `remindctl add --location`.
    pub address: Address,
    /// The trigger radius in meters.
    pub radius: u32,
}

/// A Mail account identifier, the `ZIDENTIFIER` in `Accounts4.sqlite`, stored uppercase.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(String);

impl AccountId {
    /// Accepts a full hyphenated UUID in either case.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        if !is_full_uuid(value) {
            return Err("must be a full UUID");
        }
        Ok(Self(value.to_ascii_uppercase()))
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The name clients use for a configured Mail account.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct AccountName(String);

impl AccountName {
    /// Accepts 1 to 40 lowercase letters, digits and `-`, starting with a letter or digit.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        check_name(value)?;
        Ok(Self(value.to_owned()))
    }

    /// Returns the name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Mail account clients may read, and the name they see it under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailAccount {
    /// The account uuid in Mail's mailbox URLs.
    pub id: AccountId,
    /// The name clients use.
    pub name: AccountName,
}

/// The `[mail]` table; its presence turns mail on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailConfig {
    /// The accounts clients may read, sorted by name; every other account is invisible.
    pub accounts: Vec<MailAccount>,
    /// Decoded mailbox paths never shown, compared case-insensitively.
    pub exclude_mailboxes: Vec<String>,
    /// An explicit `~/Library/Mail/V<n>` directory; `None` means the highest one with an index.
    pub root: Option<PathBuf>,
}

/// Why the mail root could not be found.
#[derive(Debug, thiserror::Error)]
pub enum MailRootError {
    /// `$HOME` is unset or empty, so `~/Library/Mail` is unknown.
    #[error("HOME is not set, cannot locate ~/Library/Mail")]
    NoHome,
    /// The mail directory could not be listed, which is how a missing Full Disk Access grant shows up.
    #[error("cannot list {}: {source}", path.display())]
    List {
        /// The directory that could not be listed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// No `V<n>` directory holds `MailData/Envelope Index`.
    #[error("no V<n> directory with MailData/Envelope Index under {}", .0.display())]
    NotFound(PathBuf),
}

impl MailConfig {
    /// The account configured under `id`, if any.
    pub fn account(&self, id: &AccountId) -> Option<&MailAccount> {
        self.accounts.iter().find(|account| account.id == *id)
    }

    /// Whether the decoded mailbox `path` is excluded, ignoring case.
    pub fn is_excluded(&self, path: &str) -> bool {
        let path = path.to_lowercase();
        for excluded in &self.exclude_mailboxes {
            if excluded.to_lowercase() == path {
                return true;
            }
        }
        false
    }

    /// The mail data directory: the configured `root`, or the highest `~/Library/Mail/V<n>` with an index.
    pub fn root(&self) -> Result<PathBuf, MailRootError> {
        if let Some(root) = &self.root {
            return Ok(root.clone());
        }
        mail_root_under_home(env::var_os("HOME"))
    }
}

fn mail_root_under_home(home: Option<OsString>) -> Result<PathBuf, MailRootError> {
    let Some(home) = home else {
        return Err(MailRootError::NoHome);
    };
    if home.is_empty() {
        return Err(MailRootError::NoHome);
    }
    discover_mail_root(&PathBuf::from(home).join(MAIL_RELATIVE_PATH))
}

/// The highest `V<n>` directory under `mail` that contains `MailData/Envelope Index`.
pub fn discover_mail_root(mail: &Path) -> Result<PathBuf, MailRootError> {
    let entries = match fs::read_dir(mail) {
        Ok(entries) => entries,
        Err(source) => {
            return Err(MailRootError::List {
                path: mail.to_owned(),
                source,
            });
        }
    };
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                return Err(MailRootError::List {
                    path: mail.to_owned(),
                    source,
                });
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(version) = name.strip_prefix('V') else {
            continue;
        };
        if !is_decimal(version) {
            continue;
        }
        let Ok(version) = version.parse::<u32>() else {
            continue;
        };
        let path = entry.path();
        if !path.join(MAIL_INDEX_RELATIVE_PATH).is_file() {
            continue;
        }
        if let Some((highest, _)) = &best
            && *highest >= version
        {
            continue;
        }
        best = Some((version, path));
    }
    let Some((_, root)) = best else {
        return Err(MailRootError::NotFound(mail.to_owned()));
    };
    Ok(root)
}

/// A DNS name clients may use to reach the bridge, stored lowercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostName(String);

impl HostName {
    /// Accepts a DNS name of letters, digits, `-` and `.`, compared case-insensitively.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        if value.is_empty() {
            return Err("empty");
        }
        for c in value.chars() {
            if !c.is_ascii_alphanumeric() && c != '-' && c != '.' {
                return Err("must contain only letters, digits, `-` and `.`");
            }
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The validated bridge configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The literal socket address the HTTP server binds to.
    pub listen: SocketAddr,
    /// Extra names the `Host` header may carry besides the listen IP.
    pub hosts: Vec<HostName>,
    /// The calendars reads may touch, as listed in the config file.
    pub read_calendars: Vec<CalendarId>,
    /// The calendars writes may touch; empty refuses every write.
    pub write_calendars: Vec<CalendarId>,
    /// An explicit `ekctl` path; `None` means `ekctl` next to the running executable.
    pub ekctl: Option<PathBuf>,
    /// The reminder lists reads may touch, as listed in the config file.
    pub read_lists: Vec<ListId>,
    /// The reminder lists writes may touch; empty refuses every reminder write.
    pub write_lists: Vec<ListId>,
    /// The places location triggers may name, sorted by name.
    pub places: Vec<Place>,
    /// An explicit `remindctl` path; `None` means `remindctl` next to the running executable.
    pub remindctl: Option<PathBuf>,
    /// The `[mail]` table; `None` turns mail off.
    pub mail: Option<MailConfig>,
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
    /// The config file is not valid TOML or has an unknown key or a wrong type; the text names only the position, never the source line.
    #[error("invalid config: {0}")]
    Parse(String),
    /// `listen` is absent.
    #[error("`listen` is required")]
    MissingListen,
    /// `listen` is not a literal `IP:port`.
    #[error("`listen` must be a literal IP:port, got {0:?}")]
    ListenNotLiteral(String),
    /// `listen` names an address that would bind every interface.
    #[error("`listen` must not be an unspecified address, got {0}")]
    ListenUnspecified(SocketAddr),
    /// A calendar id in `read_calendars` or `write_calendars` is unusable.
    #[error("`{key}` contains an invalid calendar id {value:?}: {reason}")]
    InvalidCalendarId {
        /// The config key holding the id.
        key: &'static str,
        /// The id as written.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A name in `hosts` is not a DNS name.
    #[error("`hosts` contains an invalid name {value:?}: {reason}")]
    InvalidHost {
        /// The name as written.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// `ekctl` is set to an empty path.
    #[error("`ekctl` must not be empty")]
    EmptyEkctl,
    /// A list id in `read_lists` or `write_lists` is not a full UUID.
    #[error("`{key}` contains an invalid list id {value:?}: {reason}")]
    InvalidListId {
        /// The config key holding the id.
        key: &'static str,
        /// The id as written.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A key in `[places]` is not a valid place name.
    #[error("`places` contains an invalid name {value:?}: {reason}")]
    InvalidPlaceName {
        /// The name as written.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// `places` is not a table.
    #[error("`places` must be a table")]
    PlacesNotTable,
    /// A place is not a table.
    #[error("`places.{place}` must be a table with an `address`")]
    PlaceNotTable {
        /// The place that is not a table.
        place: PlaceName,
    },
    /// A place has a key other than `address` and `radius`.
    #[error("`places.{place}` has an unknown key {key:?}, expected `address` or `radius`")]
    UnknownPlaceKey {
        /// The place with the unknown key.
        place: PlaceName,
        /// The key as written.
        key: String,
    },
    /// A place's `address` or `radius` has the wrong type.
    #[error("`places.{place}.{key}` must be {expected}")]
    PlaceKeyType {
        /// The place whose key has the wrong type.
        place: PlaceName,
        /// The key with the wrong type.
        key: &'static str,
        /// The type the key must have.
        expected: &'static str,
    },
    /// A place has no `address`.
    #[error("`places.{place}.address` is required")]
    MissingAddress {
        /// The place without an address.
        place: PlaceName,
    },
    /// A place's `address` is blank.
    #[error("`places.{place}.address` must not be empty")]
    EmptyAddress {
        /// The place whose address is blank.
        place: PlaceName,
    },
    /// A place's `address` contains a control character.
    #[error("`places.{place}.address` must not contain a control character")]
    AddressControlCharacter {
        /// The place whose address is unusable.
        place: PlaceName,
    },
    /// A place's `radius` is outside 50-2000 meters.
    #[error("`places.{place}.radius` must be between 50 and 2000 meters, got {radius}")]
    RadiusOutOfRange {
        /// The place whose radius is out of range.
        place: PlaceName,
        /// The radius as written.
        radius: i64,
    },
    /// `remindctl` is set to an empty path.
    #[error("`remindctl` must not be empty")]
    EmptyRemindctl,
    /// A key in `[mail.accounts]` is not a full UUID.
    #[error("`mail.accounts` contains an invalid account id {value:?}: {reason}")]
    InvalidMailAccountId {
        /// The id as written.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A value in `[mail.accounts]` is not a valid account name.
    #[error("`mail.accounts.{id}` has an invalid name {value:?}: {reason}")]
    InvalidMailAccountName {
        /// The account the name is given to.
        id: AccountId,
        /// The name as written.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Two keys in `[mail.accounts]` name the same account uuid in different case.
    #[error("`mail.accounts` lists the account {0} twice")]
    DuplicateMailAccountId(AccountId),
    /// Two accounts in `[mail.accounts]` share a name.
    #[error("`mail.accounts` gives the name {0:?} to more than one account")]
    DuplicateMailAccountName(AccountName),
    /// An entry in `mail.exclude_mailboxes` is blank.
    #[error("`mail.exclude_mailboxes` must not contain an empty path")]
    EmptyExcludedMailbox,
    /// `mail.root` is set to an empty path.
    #[error("`mail.root` must not be empty")]
    EmptyMailRoot,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: Option<String>,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    read_calendars: Vec<String>,
    #[serde(default)]
    write_calendars: Vec<String>,
    ekctl: Option<PathBuf>,
    #[serde(default)]
    read_lists: Vec<String>,
    #[serde(default)]
    write_lists: Vec<String>,
    places: Option<toml::Value>,
    remindctl: Option<PathBuf>,
    mail: Option<RawMail>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMail {
    exclude_mailboxes: Option<Vec<String>>,
    root: Option<PathBuf>,
    #[serde(default)]
    accounts: BTreeMap<String, String>,
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
            hosts,
            read_calendars,
            write_calendars,
            ekctl,
            read_lists,
            write_lists,
            places,
            remindctl,
            mail,
        } = match toml::from_str(text) {
            Ok(raw) => raw,
            Err(err) => return Err(parse_error(text, &err)),
        };

        let Some(listen) = listen else {
            return Err(ConfigError::MissingListen);
        };
        let listen = parse_listen(&listen)?;

        let mut names = Vec::new();
        for host in hosts {
            let name = match HostName::parse(&host) {
                Ok(name) => name,
                Err(reason) => {
                    return Err(ConfigError::InvalidHost {
                        value: host,
                        reason,
                    });
                }
            };
            names.push(name);
        }

        let mut read = Vec::new();
        for id in read_calendars {
            read.push(parse_calendar_id("read_calendars", id)?);
        }

        let mut write = Vec::new();
        for id in write_calendars {
            write.push(parse_calendar_id("write_calendars", id)?);
        }

        if let Some(path) = &ekctl
            && path.as_os_str().is_empty()
        {
            return Err(ConfigError::EmptyEkctl);
        }

        let mut lists_read = Vec::new();
        for id in read_lists {
            lists_read.push(parse_list_id("read_lists", id)?);
        }

        let mut lists_write = Vec::new();
        for id in write_lists {
            lists_write.push(parse_list_id("write_lists", id)?);
        }

        let places = match places {
            None => toml::Table::new(),
            Some(toml::Value::Table(table)) => table,
            Some(
                toml::Value::String(_)
                | toml::Value::Integer(_)
                | toml::Value::Float(_)
                | toml::Value::Boolean(_)
                | toml::Value::Datetime(_)
                | toml::Value::Array(_),
            ) => return Err(ConfigError::PlacesNotTable),
        };
        let mut configured = Vec::new();
        for (name, place) in places {
            configured.push(parse_place(name, place)?);
        }

        if let Some(path) = &remindctl
            && path.as_os_str().is_empty()
        {
            return Err(ConfigError::EmptyRemindctl);
        }

        let mail = match mail {
            None => None,
            Some(mail) => Some(parse_mail(mail)?),
        };

        Ok(Self {
            listen,
            hosts: names,
            read_calendars: read,
            write_calendars: write,
            ekctl,
            read_lists: lists_read,
            write_lists: lists_write,
            places: configured,
            remindctl,
            mail,
        })
    }

    /// Every calendar reads may touch: `read_calendars` plus `write_calendars`, without duplicates.
    pub fn readable_calendars(&self) -> Vec<CalendarId> {
        let mut readable = Vec::new();
        for id in self
            .read_calendars
            .iter()
            .chain(self.write_calendars.iter())
        {
            if !readable.contains(id) {
                readable.push(id.clone());
            }
        }
        readable
    }

    /// Every reminder list reads may touch: `read_lists` plus `write_lists`, without duplicates.
    pub fn readable_lists(&self) -> Vec<ListId> {
        let mut readable = Vec::new();
        for id in self.read_lists.iter().chain(self.write_lists.iter()) {
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
        beside(executable, EKCTL_FILE_NAME)
    }

    /// The `remindctl` to run: the configured path, or `remindctl` beside `executable`.
    pub fn remindctl_path(&self, executable: &Path) -> PathBuf {
        if let Some(path) = &self.remindctl {
            return path.clone();
        }
        beside(executable, REMINDCTL_FILE_NAME)
    }
}

fn beside(executable: &Path, file_name: &str) -> PathBuf {
    let Some(dir) = executable.parent() else {
        return PathBuf::from(file_name);
    };
    dir.join(file_name)
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
    if addr.ip().to_canonical().is_unspecified() {
        return Err(ConfigError::ListenUnspecified(addr));
    }
    Ok(addr)
}

fn parse_calendar_id(key: &'static str, value: String) -> Result<CalendarId, ConfigError> {
    match CalendarId::parse(value.clone()) {
        Ok(id) => Ok(id),
        Err(reason) => Err(ConfigError::InvalidCalendarId { key, value, reason }),
    }
}

fn parse_list_id(key: &'static str, value: String) -> Result<ListId, ConfigError> {
    match ListId::parse(value.clone()) {
        Ok(id) => Ok(id),
        Err(reason) => Err(ConfigError::InvalidListId { key, value, reason }),
    }
}

fn parse_error(text: &str, err: &toml::de::Error) -> ConfigError {
    let message = err.message();
    let Some(span) = err.span() else {
        return ConfigError::Parse(message.to_owned());
    };
    let mut line = 1;
    let mut column = 1;
    for c in text.get(..span.start).unwrap_or(text).chars() {
        if c == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    ConfigError::Parse(format!("line {line}, column {column}: {message}"))
}

fn parse_place(name: String, place: toml::Value) -> Result<Place, ConfigError> {
    let name = match PlaceName::parse(&name) {
        Ok(parsed) => parsed,
        Err(reason) => {
            return Err(ConfigError::InvalidPlaceName {
                value: name,
                reason,
            });
        }
    };
    let toml::Value::Table(place) = place else {
        return Err(ConfigError::PlaceNotTable { place: name });
    };
    let mut address = None;
    let mut radius = None;
    for (key, value) in place {
        match key.as_str() {
            "address" => {
                let toml::Value::String(value) = value else {
                    return Err(ConfigError::PlaceKeyType {
                        place: name,
                        key: "address",
                        expected: "a string",
                    });
                };
                address = Some(value);
            }
            "radius" => {
                let toml::Value::Integer(value) = value else {
                    return Err(ConfigError::PlaceKeyType {
                        place: name,
                        key: "radius",
                        expected: "an integer",
                    });
                };
                radius = Some(value);
            }
            _ => return Err(ConfigError::UnknownPlaceKey { place: name, key }),
        }
    }
    let Some(address) = address else {
        return Err(ConfigError::MissingAddress { place: name });
    };
    if address.trim().is_empty() {
        return Err(ConfigError::EmptyAddress { place: name });
    }
    if has_control_character(&address) {
        return Err(ConfigError::AddressControlCharacter { place: name });
    }
    let radius = match radius {
        None => DEFAULT_RADIUS,
        Some(radius) => match u32::try_from(radius) {
            Ok(meters) if (MIN_RADIUS..=MAX_RADIUS).contains(&meters) => meters,
            Ok(_) | Err(_) => {
                return Err(ConfigError::RadiusOutOfRange {
                    place: name,
                    radius,
                });
            }
        },
    };
    Ok(Place {
        name,
        address: Address(address),
        radius,
    })
}

fn parse_mail(mail: RawMail) -> Result<MailConfig, ConfigError> {
    let RawMail {
        exclude_mailboxes,
        root,
        accounts,
    } = mail;
    let mut configured: Vec<MailAccount> = Vec::new();
    for (id, name) in accounts {
        let id = match AccountId::parse(&id) {
            Ok(parsed) => parsed,
            Err(reason) => return Err(ConfigError::InvalidMailAccountId { value: id, reason }),
        };
        let name = match AccountName::parse(&name) {
            Ok(parsed) => parsed,
            Err(reason) => {
                return Err(ConfigError::InvalidMailAccountName {
                    id,
                    value: name,
                    reason,
                });
            }
        };
        for account in &configured {
            if account.id == id {
                return Err(ConfigError::DuplicateMailAccountId(id));
            }
            if account.name == name {
                return Err(ConfigError::DuplicateMailAccountName(name));
            }
        }
        configured.push(MailAccount { id, name });
    }
    configured.sort_by(|a, b| a.name.cmp(&b.name));

    let exclude_mailboxes = match exclude_mailboxes {
        Some(paths) => paths,
        None => {
            let mut paths = Vec::new();
            for path in DEFAULT_EXCLUDED_MAILBOXES {
                paths.push(path.to_owned());
            }
            paths
        }
    };
    for path in &exclude_mailboxes {
        if path.trim().is_empty() {
            return Err(ConfigError::EmptyExcludedMailbox);
        }
    }

    if let Some(path) = &root
        && path.as_os_str().is_empty()
    {
        return Err(ConfigError::EmptyMailRoot);
    }

    Ok(MailConfig {
        accounts: configured,
        exclude_mailboxes,
        root,
    })
}

pub(crate) fn is_decimal(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    for c in value.chars() {
        if !c.is_ascii_digit() {
            return false;
        }
    }
    true
}

fn check_name(value: &str) -> Result<(), &'static str> {
    let Some(first) = value.chars().next() else {
        return Err("empty");
    };
    if value.len() > NAME_MAX_LEN {
        return Err("longer than 40 characters");
    }
    if first == '-' {
        return Err("must start with a letter or digit");
    }
    for c in value.chars() {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '-' {
            return Err("must contain only lowercase letters, digits and `-`");
        }
    }
    Ok(())
}

/// Whether `value` is a full hyphenated UUID, the only id form `remindctl` never reads as a row index.
pub fn is_full_uuid(value: &str) -> bool {
    let mut groups = 0;
    for group in value.split('-') {
        let Some(expected) = UUID_GROUP_LENGTHS.get(groups) else {
            return false;
        };
        if group.len() != *expected {
            return false;
        }
        for c in group.chars() {
            if !c.is_ascii_hexdigit() {
                return false;
            }
        }
        groups += 1;
    }
    groups == UUID_GROUP_LENGTHS.len()
}

pub(crate) fn has_control_character(value: &str) -> bool {
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
    const OTHER_ID: &str = "11111111-2222-3333-4444-555555555555";

    fn id(value: &str) -> CalendarId {
        CalendarId(value.to_owned())
    }

    #[test]
    fn valid_config() {
        let config = Config::from_toml(&format!(
            r#"
            listen = "100.64.0.1:8790"
            hosts = ["Mac.tail1234.ts.net"]
            read_calendars = ["{READ_ID}"]
            write_calendars = ["{WRITE_ID}"]
            ekctl = "/opt/ekctl"
            "#
        ))
        .unwrap();

        assert_eq!(
            config,
            Config {
                listen: "100.64.0.1:8790".parse().unwrap(),
                hosts: vec![HostName("mac.tail1234.ts.net".to_owned())],
                read_calendars: vec![id(READ_ID)],
                write_calendars: vec![id(WRITE_ID)],
                ekctl: Some(PathBuf::from("/opt/ekctl")),
                read_lists: Vec::new(),
                write_lists: Vec::new(),
                places: Vec::new(),
                remindctl: None,
                mail: None,
            }
        );
    }

    fn list(value: &str) -> ListId {
        ListId(value.to_owned())
    }

    fn place(name: &str, address: &str, radius: u32) -> Place {
        Place {
            name: PlaceName(name.to_owned()),
            address: Address(address.to_owned()),
            radius,
        }
    }

    fn with_places(places: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\n[places]\n{places}\n"
        ))
    }

    #[test]
    fn valid_reminders_config() {
        let config = Config::from_toml(&format!(
            r#"
            listen = "127.0.0.1:8790"
            read_lists = ["{READ_ID}"]
            write_lists = ["{WRITE_ID}"]
            remindctl = "/opt/remindctl"

            [places]
            shop = {{ address = "1 Market Street, Springfield", radius = 150 }}
            home-2 = {{ address = "2 Elm Street, Springfield" }}
            "#
        ))
        .unwrap();

        assert_eq!(config.read_lists, vec![list(READ_ID)]);
        assert_eq!(config.write_lists, vec![list(WRITE_ID)]);
        assert_eq!(
            config.places,
            vec![
                place("home-2", "2 Elm Street, Springfield", DEFAULT_RADIUS),
                place("shop", "1 Market Street, Springfield", 150),
            ]
        );
        assert_eq!(config.remindctl, Some(PathBuf::from("/opt/remindctl")));
    }

    #[test]
    fn minimal_config_has_no_reminders() {
        let config = Config::from_toml(r#"listen = "127.0.0.1:8790""#).unwrap();
        assert!(config.read_lists.is_empty());
        assert!(config.write_lists.is_empty());
        assert!(config.places.is_empty());
        assert_eq!(config.remindctl, None);
        assert!(config.readable_lists().is_empty());
    }

    #[test]
    fn invalid_list_ids() {
        for (key, value) in [
            ("read_lists", "1"),
            ("read_lists", "4F7D9489"),
            ("write_lists", "4F7D9489-A78F-4369-A951-213207DCFEE"),
            ("write_lists", "4F7D9489-A78F-4369-A951-213207DCFEE3-0"),
            ("read_lists", "4F7D9489A78F-4369-A951-213207DCFEE3-"),
            ("read_lists", "ZF7D9489-A78F-4369-A951-213207DCFEE3"),
            ("write_lists", ""),
        ] {
            let err = Config::from_toml(&format!(
                "listen = \"127.0.0.1:8790\"\n{key} = [\"{value}\"]"
            ))
            .unwrap_err();
            let ConfigError::InvalidListId {
                key: named,
                value: written,
                reason,
            } = &err
            else {
                panic!("unexpected error: {err:?}");
            };
            assert_eq!(*named, key);
            assert_eq!(written, value);
            assert_eq!(*reason, "must be a full UUID");
            assert!(err.to_string().contains(&format!("`{key}`")), "{err}");
        }
    }

    #[test]
    fn lowercase_list_id_is_stored_in_uppercase() {
        let lower = READ_ID.to_ascii_lowercase();
        let config = Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\nread_lists = [\"{lower}\"]"
        ))
        .unwrap();
        assert_eq!(config.read_lists, vec![list(READ_ID)]);
    }

    #[test]
    fn invalid_place_names() {
        for (name, expected) in [
            ("\"\"", "empty"),
            (
                "Shop",
                "must contain only lowercase letters, digits and `-`",
            ),
            (
                "my_shop",
                "must contain only lowercase letters, digits and `-`",
            ),
            (
                "\"my shop\"",
                "must contain only lowercase letters, digits and `-`",
            ),
            ("-shop", "must start with a letter or digit"),
            (
                "a1234567890123456789012345678901234567890",
                "longer than 40 characters",
            ),
        ] {
            let err = with_places(&format!("{name} = {{ address = \"1 Main St\" }}")).unwrap_err();
            let ConfigError::InvalidPlaceName { reason, .. } = &err else {
                panic!("unexpected error: {err:?}");
            };
            assert_eq!(*reason, expected, "{name}");
            assert!(err.to_string().contains("`places`"), "{err}");
        }
    }

    #[test]
    fn longest_place_name() {
        let name = format!("9{}b", "a-".repeat(19));
        assert_eq!(name.len(), 40);
        let config = with_places(&format!("{name} = {{ address = \"1 Main St\" }}")).unwrap();
        assert_eq!(config.places[0].name.as_str(), name);
    }

    #[test]
    fn empty_address() {
        for address in ["", "  "] {
            let err = with_places(&format!("shop = {{ address = \"{address}\" }}")).unwrap_err();
            let ConfigError::EmptyAddress { place } = &err else {
                panic!("unexpected error: {err:?}");
            };
            assert_eq!(place.as_str(), "shop");
            assert_eq!(err.to_string(), "`places.shop.address` must not be empty");
        }
    }

    #[test]
    fn address_with_control_character() {
        let err = with_places(r#"shop = { address = "1 Main St\u0007" }"#).unwrap_err();
        let ConfigError::AddressControlCharacter { place } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(place.as_str(), "shop");
        assert!(!err.to_string().contains("Main"), "{err}");
    }

    #[test]
    fn missing_address() {
        let err = with_places("shop = { radius = 100 }").unwrap_err();
        let ConfigError::MissingAddress { place } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(place.as_str(), "shop");
        assert!(err.to_string().contains("`places.shop.address`"), "{err}");
    }

    #[test]
    fn unknown_place_key() {
        let err = with_places(r#"shop = { address = "1 Main St", lat = 1 }"#).unwrap_err();
        let ConfigError::UnknownPlaceKey { place, key } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(place.as_str(), "shop");
        assert_eq!(key, "lat");
        assert!(!err.to_string().contains("Main"), "{err}");
    }

    #[test]
    fn radius_wrong_type() {
        let err = with_places(r#"shop = { address = "1 Main St", radius = "150" }"#).unwrap_err();
        let ConfigError::PlaceKeyType {
            place,
            key,
            expected,
        } = &err
        else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(place.as_str(), "shop");
        assert_eq!(*key, "radius");
        assert_eq!(*expected, "an integer");
        assert!(!err.to_string().contains("Main"), "{err}");
    }

    #[test]
    fn address_wrong_type() {
        let err = with_places("shop = { address = [\"1 Main St\"] }").unwrap_err();
        let ConfigError::PlaceKeyType { place, key, .. } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(place.as_str(), "shop");
        assert_eq!(*key, "address");
        assert!(!err.to_string().contains("Main"), "{err}");
    }

    #[test]
    fn place_not_table() {
        let err = with_places(r#"shop = "1 Main St""#).unwrap_err();
        let ConfigError::PlaceNotTable { place } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(place.as_str(), "shop");
        assert!(!err.to_string().contains("Main"), "{err}");
    }

    #[test]
    fn places_not_table() {
        let err =
            Config::from_toml("listen = \"127.0.0.1:8790\"\nplaces = \"1 Main St\"").unwrap_err();
        let ConfigError::PlacesNotTable = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(!err.to_string().contains("Main"), "{err}");
    }

    #[test]
    fn place_syntax_error_hides_address() {
        for places in [
            r#"shop = { address = "1 Main St", radius = 150"#,
            r#"shop = { address = "1 Main St\q" }"#,
            r#"shop = { address = 1 Main St }"#,
            r#"shop = { address = "1 Main St", address = "2 Main St" }"#,
        ] {
            let err = with_places(places).unwrap_err();
            let ConfigError::Parse(_) = &err else {
                panic!("unexpected error: {err:?}");
            };
            let message = err.to_string();
            assert!(message.contains("line 3, column "), "{message}");
            assert!(!message.contains("Main"), "{message}");
        }
    }

    #[test]
    fn radius_out_of_range() {
        for radius in [-1, 0, 49, 2001, 5_000_000_000] {
            let err = with_places(&format!(
                "shop = {{ address = \"1 Main St\", radius = {radius} }}"
            ))
            .unwrap_err();
            let ConfigError::RadiusOutOfRange { place, radius: got } = &err else {
                panic!("unexpected error: {err:?}");
            };
            assert_eq!(place.as_str(), "shop");
            assert_eq!(*got, radius);
            assert!(err.to_string().contains("`places.shop.radius`"), "{err}");
        }
    }

    #[test]
    fn radius_bounds_are_inclusive() {
        let config = with_places(
            "near = { address = \"1 Main St\", radius = 50 }\nfar = { address = \"2 Main St\", radius = 2000 }",
        )
        .unwrap();
        assert_eq!(
            config.places,
            vec![
                place("far", "2 Main St", 2000),
                place("near", "1 Main St", 50)
            ]
        );
    }

    #[test]
    fn default_radius() {
        let config = with_places(r#"home = { address = "2 Elm Street" }"#).unwrap();
        assert_eq!(config.places, vec![place("home", "2 Elm Street", 100)]);
    }

    #[test]
    fn address_is_redacted_in_debug() {
        let config = with_places(r#"home = { address = "2 Elm Street" }"#).unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("Elm"), "{debug}");
        assert!(debug.contains("home"), "{debug}");
    }

    #[test]
    fn write_list_implied_readable() {
        let config = Config::from_toml(&format!(
            r#"
            listen = "127.0.0.1:8790"
            read_lists = ["{WRITE_ID}", "{READ_ID}", "{READ_ID}"]
            write_lists = ["{WRITE_ID}", "{OTHER_ID}"]
            "#
        ))
        .unwrap();
        assert_eq!(
            config.readable_lists(),
            vec![list(WRITE_ID), list(READ_ID), list(OTHER_ID)]
        );
    }

    #[test]
    fn write_list_readable_without_read_lists() {
        let config = Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\nwrite_lists = [\"{WRITE_ID}\"]"
        ))
        .unwrap();
        assert!(config.read_lists.is_empty());
        assert_eq!(config.readable_lists(), vec![list(WRITE_ID)]);
    }

    #[test]
    fn empty_remindctl() {
        let err = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            remindctl = ""
            "#,
        )
        .unwrap_err();
        let ConfigError::EmptyRemindctl = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[test]
    fn remindctl_defaults_next_to_executable() {
        let config = Config::from_toml(r#"listen = "127.0.0.1:8790""#).unwrap();
        let executable =
            Path::new("/Applications/EventKitBridge.app/Contents/MacOS/eventkit-bridge");
        assert_eq!(
            config.remindctl_path(executable),
            PathBuf::from("/Applications/EventKitBridge.app/Contents/MacOS/remindctl")
        );
    }

    #[test]
    fn remindctl_override() {
        let config = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            remindctl = "/usr/local/bin/remindctl"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.remindctl_path(Path::new("/somewhere/eventkit-bridge")),
            PathBuf::from("/usr/local/bin/remindctl")
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
        assert!(config.hosts.is_empty());
        assert!(config.read_calendars.is_empty());
        assert!(config.write_calendars.is_empty());
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
    fn ipv4_mapped_unspecified_listen() {
        for value in [
            r#"listen = "[::ffff:0.0.0.0]:8790""#,
            r#"listen = "[::ffff:0:0]:8790""#,
        ] {
            let err = Config::from_toml(value).unwrap_err();
            let ConfigError::ListenUnspecified(_) = err else {
                panic!("unexpected error: {err:?}");
            };
        }
    }

    #[test]
    fn ipv4_mapped_loopback_listen() {
        let config = Config::from_toml(r#"listen = "[::ffff:127.0.0.1]:8790""#).unwrap();
        assert_eq!(config.listen, "[::ffff:127.0.0.1]:8790".parse().unwrap());
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
        let err = Config::from_toml(r#"listen = "100.64.0.1""#).unwrap_err();
        let ConfigError::ListenNotLiteral(_) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[test]
    fn malformed_toml() {
        let err = Config::from_toml(r#"listen = "127.0.0.1:8790"#).unwrap_err();
        let ConfigError::Parse(_) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(err.to_string().contains("line 1, column "), "{err}");
    }

    #[test]
    fn wrong_type_names_position() {
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
        assert!(err.to_string().contains("line 3, column 30"), "{err}");
    }

    #[test]
    fn unknown_key_names_key() {
        let err = Config::from_toml(
            r#"
            listen = "127.0.0.1:8790"
            write_calendar = "x"
            "#,
        )
        .unwrap_err();
        let ConfigError::Parse(_) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(err.to_string().contains("write_calendar"), "{err}");
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
            write_calendars = ["{READ_ID},{WRITE_ID}"]
            "#
        ))
        .unwrap_err();
        let ConfigError::InvalidCalendarId { key, reason, .. } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(*key, "write_calendars");
        assert_eq!(*reason, "contains a comma");
        assert!(err.to_string().contains("`write_calendars`"));
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
    fn invalid_hosts() {
        for (host, expected) in [
            ("", "empty"),
            ("mac:8790", "must contain only letters, digits, `-` and `.`"),
            (
                "evil.com/x",
                "must contain only letters, digits, `-` and `.`",
            ),
            ("[::1]", "must contain only letters, digits, `-` and `.`"),
        ] {
            let err = Config::from_toml(&format!(
                "listen = \"127.0.0.1:8790\"\nhosts = [\"{host}\"]"
            ))
            .unwrap_err();
            let ConfigError::InvalidHost { value, reason } = &err else {
                panic!("unexpected error: {err:?}");
            };
            assert_eq!(value, host);
            assert_eq!(*reason, expected, "{host}");
            assert!(err.to_string().contains("`hosts`"), "{err}");
        }
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
            write_calendars = ["{WRITE_ID}"]
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
            write_calendars = ["{WRITE_ID}"]
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
            write_calendars = ["{WRITE_ID}"]
            "#
        ))
        .unwrap();
        assert_eq!(config.readable_calendars(), vec![id(WRITE_ID), id(READ_ID)]);
    }

    #[test]
    fn absent_write_calendars() {
        let config = Config::from_toml(&format!(
            r#"
            listen = "127.0.0.1:8790"
            read_calendars = ["{READ_ID}"]
            "#
        ))
        .unwrap();
        assert!(config.write_calendars.is_empty());
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

    fn with_mail(mail: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&format!("listen = \"127.0.0.1:8790\"\n[mail]\n{mail}\n"))
    }

    fn mail_account(id: &str, name: &str) -> MailAccount {
        MailAccount {
            id: AccountId(id.to_owned()),
            name: AccountName(name.to_owned()),
        }
    }

    #[test]
    fn valid_mail_config() {
        let config = with_mail(&format!(
            r#"
            exclude_mailboxes = ["Trash", "Archive/2019"]
            root = "/tmp/Mail/V10"

            [mail.accounts]
            "{READ_ID}" = "main"
            "{}" = "gmail-2"
            "#,
            WRITE_ID.to_ascii_lowercase()
        ))
        .unwrap();
        assert_eq!(
            config.mail,
            Some(MailConfig {
                accounts: vec![
                    mail_account(WRITE_ID, "gmail-2"),
                    mail_account(READ_ID, "main"),
                ],
                exclude_mailboxes: vec!["Trash".to_owned(), "Archive/2019".to_owned()],
                root: Some(PathBuf::from("/tmp/Mail/V10")),
            })
        );
    }

    #[test]
    fn absent_mail_is_off() {
        let config = Config::from_toml(r#"listen = "127.0.0.1:8790""#).unwrap();
        assert_eq!(config.mail, None);
    }

    #[test]
    fn empty_mail_table_uses_defaults() {
        let config = with_mail("").unwrap();
        let mail = config.mail.unwrap();
        assert!(mail.accounts.is_empty());
        assert_eq!(mail.exclude_mailboxes, DEFAULT_EXCLUDED_MAILBOXES);
        assert_eq!(mail.root, None);
    }

    #[test]
    fn empty_exclude_mailboxes_excludes_nothing() {
        let mail = with_mail("exclude_mailboxes = []").unwrap().mail.unwrap();
        assert!(mail.exclude_mailboxes.is_empty());
        assert!(!mail.is_excluded("Trash"));
    }

    #[test]
    fn invalid_mail_account_names() {
        for (name, expected) in [
            ("", "empty"),
            (
                "Main",
                "must contain only lowercase letters, digits and `-`",
            ),
            (
                "my_mail",
                "must contain only lowercase letters, digits and `-`",
            ),
            (
                "my mail",
                "must contain only lowercase letters, digits and `-`",
            ),
            (
                "почта",
                "must contain only lowercase letters, digits and `-`",
            ),
            ("-main", "must start with a letter or digit"),
            (
                "a1234567890123456789012345678901234567890",
                "longer than 40 characters",
            ),
        ] {
            let err =
                with_mail(&format!("[mail.accounts]\n\"{READ_ID}\" = \"{name}\"")).unwrap_err();
            let ConfigError::InvalidMailAccountName { id, value, reason } = &err else {
                panic!("unexpected error: {err:?}");
            };
            assert_eq!(id.as_str(), READ_ID);
            assert_eq!(value, name);
            assert_eq!(*reason, expected, "{name}");
            assert!(
                err.to_string()
                    .contains(&format!("`mail.accounts.{READ_ID}`")),
                "{err}"
            );
        }
    }

    #[test]
    fn longest_mail_account_name() {
        let name = format!("9{}b", "a-".repeat(19));
        let mail = with_mail(&format!("[mail.accounts]\n\"{READ_ID}\" = \"{name}\""))
            .unwrap()
            .mail
            .unwrap();
        assert_eq!(mail.accounts[0].name.as_str(), name);
    }

    #[test]
    fn invalid_mail_account_ids() {
        for value in [
            "main",
            "",
            "4F7D9489",
            "4F7D9489-A78F-4369-A951-213207DCFEEZ",
        ] {
            let err = with_mail(&format!("[mail.accounts]\n\"{value}\" = \"main\"")).unwrap_err();
            let ConfigError::InvalidMailAccountId {
                value: written,
                reason,
            } = &err
            else {
                panic!("unexpected error: {err:?}");
            };
            assert_eq!(written, value);
            assert_eq!(*reason, "must be a full UUID");
            assert!(err.to_string().contains("`mail.accounts`"), "{err}");
        }
    }

    #[test]
    fn duplicate_mail_account_name() {
        let err = with_mail(&format!(
            "[mail.accounts]\n\"{READ_ID}\" = \"main\"\n\"{WRITE_ID}\" = \"main\""
        ))
        .unwrap_err();
        let ConfigError::DuplicateMailAccountName(name) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(name.as_str(), "main");
    }

    #[test]
    fn duplicate_mail_account_id_in_other_case() {
        let lower = READ_ID.to_ascii_lowercase();
        let err = with_mail(&format!(
            "[mail.accounts]\n\"{READ_ID}\" = \"main\"\n\"{lower}\" = \"other\""
        ))
        .unwrap_err();
        let ConfigError::DuplicateMailAccountId(id) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(id.as_str(), READ_ID);
    }

    #[test]
    fn empty_excluded_mailbox() {
        for path in ["", " "] {
            let err =
                with_mail(&format!("exclude_mailboxes = [\"Trash\", \"{path}\"]")).unwrap_err();
            let ConfigError::EmptyExcludedMailbox = err else {
                panic!("unexpected error: {err:?}");
            };
        }
    }

    #[test]
    fn empty_mail_root() {
        let err = with_mail(r#"root = """#).unwrap_err();
        let ConfigError::EmptyMailRoot = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[test]
    fn unknown_mail_key() {
        let err = with_mail("accounts_dir = \"/tmp\"").unwrap_err();
        let ConfigError::Parse(_) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(err.to_string().contains("accounts_dir"), "{err}");
    }

    #[test]
    fn mail_account_name_wrong_type() {
        let err = with_mail(&format!("[mail.accounts]\n\"{READ_ID}\" = 1")).unwrap_err();
        let ConfigError::Parse(_) = &err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[test]
    fn excluded_mailboxes_ignore_case() {
        let mail = with_mail(r#"exclude_mailboxes = ["Junk", "[Gmail]/Spam", "Корзина"]"#)
            .unwrap()
            .mail
            .unwrap();
        assert!(mail.is_excluded("Junk"));
        assert!(mail.is_excluded("JUNK"));
        assert!(mail.is_excluded("[gmail]/spam"));
        assert!(mail.is_excluded("КОРЗИНА"));
        assert!(!mail.is_excluded("Junk/Old"));
        assert!(!mail.is_excluded("Inbox"));
    }

    #[test]
    fn default_exclusions_cover_trash() {
        let mail = with_mail("").unwrap().mail.unwrap();
        assert!(mail.is_excluded("trash"));
        assert!(mail.is_excluded("[GMAIL]/TRASH"));
        assert!(!mail.is_excluded("INBOX"));
    }

    #[test]
    fn mail_account_lookup() {
        let mail = with_mail(&format!("[mail.accounts]\n\"{READ_ID}\" = \"main\""))
            .unwrap()
            .mail
            .unwrap();
        let found = mail.account(&AccountId::parse(&READ_ID.to_ascii_lowercase()).unwrap());
        assert_eq!(found, Some(&mail_account(READ_ID, "main")));
        assert_eq!(mail.account(&AccountId::parse(WRITE_ID).unwrap()), None);
    }

    fn mail_version(mail: &Path, name: &str, with_index: bool) {
        let data = mail.join(name).join("MailData");
        fs::create_dir_all(&data).unwrap();
        if with_index {
            fs::write(data.join("Envelope Index"), "").unwrap();
        }
    }

    #[test]
    fn root_discovery_picks_highest_version_with_index() {
        let dir = tempfile::tempdir().unwrap();
        let mail = dir.path();
        mail_version(mail, "V2", true);
        mail_version(mail, "V9", true);
        mail_version(mail, "V10", true);
        mail_version(mail, "V11", false);
        mail_version(mail, "V", true);
        mail_version(mail, "Vx", true);
        mail_version(mail, "V+12", true);
        mail_version(mail, "v13", true);
        mail_version(mail, "MailData", true);
        fs::write(mail.join("V14"), "").unwrap();
        fs::create_dir_all(mail.join("V15/MailData/Envelope Index")).unwrap();
        assert_eq!(discover_mail_root(mail).unwrap(), mail.join("V10"));
    }

    #[test]
    fn root_discovery_without_index() {
        let dir = tempfile::tempdir().unwrap();
        mail_version(dir.path(), "V10", false);
        let err = discover_mail_root(dir.path()).unwrap_err();
        let MailRootError::NotFound(path) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(path, dir.path());
    }

    #[test]
    fn root_discovery_unlistable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("Mail");
        let err = discover_mail_root(&missing).unwrap_err();
        let MailRootError::List { path, source } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(path, &missing);
        assert_eq!(source.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn root_discovery_under_home() {
        let dir = tempfile::tempdir().unwrap();
        mail_version(&dir.path().join("Library/Mail"), "V10", true);
        assert_eq!(
            mail_root_under_home(Some(dir.path().as_os_str().to_owned())).unwrap(),
            dir.path().join("Library/Mail/V10")
        );
    }

    #[test]
    fn root_discovery_without_home() {
        let MailRootError::NoHome = mail_root_under_home(None).unwrap_err() else {
            panic!("expected NoHome");
        };
        let MailRootError::NoHome = mail_root_under_home(Some(OsString::new())).unwrap_err() else {
            panic!("expected NoHome");
        };
    }

    #[test]
    fn configured_root_is_not_discovered() {
        let mail = with_mail(r#"root = "/nonexistent/Mail/V10""#)
            .unwrap()
            .mail
            .unwrap();
        assert_eq!(mail.root().unwrap(), PathBuf::from("/nonexistent/Mail/V10"));
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
