use std::fmt;

use chrono::{DateTime, FixedOffset, NaiveDate, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize, Serializer};

use crate::config::{ListId, Place, PlaceName, is_full_uuid};
use crate::model::Access;

/// A reminder identifier, always a full UUID so `remindctl` never reads it as a row index.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReminderId(String);

/// Why a string was rejected as a reminder id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("reminder id must be a full UUID")]
pub struct InvalidReminderId;

impl ReminderId {
    /// Accepts a full UUID such as `0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D`.
    pub fn parse(value: &str) -> Result<Self, InvalidReminderId> {
        if !is_full_uuid(value) {
            return Err(InvalidReminderId);
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ReminderId {
    type Error = InvalidReminderId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<ReminderId> for String {
    fn from(id: ReminderId) -> Self {
        id.0
    }
}

impl fmt::Display for ReminderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A reminder's priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// No priority.
    None,
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
}

impl Priority {
    /// The priority as `remindctl` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::None => "none",
            Priority::Low => "low",
            Priority::Medium => "medium",
            Priority::High => "high",
        }
    }
}

/// A repeat rule clients may set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    /// Every day.
    Daily,
    /// Every week.
    Weekly,
    /// Every two weeks.
    Biweekly,
    /// Every month.
    Monthly,
    /// Every year.
    Yearly,
}

impl Repeat {
    /// The rule as `remindctl --repeat` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Repeat::Daily => "daily",
            Repeat::Weekly => "weekly",
            Repeat::Biweekly => "biweekly",
            Repeat::Monthly => "monthly",
            Repeat::Yearly => "yearly",
        }
    }
}

/// A reminder's repeat rule as the bridge reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderRepeat {
    /// One of the rules clients may set.
    Rule(Repeat),
    /// Any other rule, left as it is.
    Custom,
}

impl Serialize for ReminderRepeat {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ReminderRepeat::Rule(repeat) => serializer.serialize_str(repeat.as_str()),
            ReminderRepeat::Custom => serializer.serialize_str("custom"),
        }
    }
}

/// When a location trigger fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Proximity {
    /// On arriving at the place.
    Arriving,
    /// On leaving the place.
    Leaving,
}

/// A reminder's due date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// A date and time; the reminder notifies then.
    At(DateTime<FixedOffset>),
    /// A whole day, without a notification.
    Day(NaiveDate),
}

impl fmt::Display for Due {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Due::At(at) => f.write_str(&at.to_rfc3339_opts(SecondsFormat::Secs, false)),
            Due::Day(day) => write!(f, "{}", day.format("%Y-%m-%d")),
        }
    }
}

impl Serialize for Due {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// An instant in the Mac's local zone, written as RFC 3339 with whole seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime(DateTime<FixedOffset>);

impl LocalTime {
    /// The instant with its local offset.
    pub fn as_datetime(self) -> DateTime<FixedOffset> {
        self.0
    }
}

impl Serialize for LocalTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_rfc3339_opts(SecondsFormat::Secs, false))
    }
}

/// An RFC 3339 instant as `remindctl` reports it, in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct RcInstant(DateTime<Utc>);

impl TryFrom<String> for RcInstant {
    type Error = chrono::ParseError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let value = DateTime::parse_from_rfc3339(&text)?;
        Ok(Self(value.with_timezone(&Utc)))
    }
}

impl RcInstant {
    fn local<Tz: TimeZone>(self, zone: &Tz) -> DateTime<FixedOffset> {
        self.0.with_timezone(zone).fixed_offset()
    }
}

/// A reminder list as `remindctl list` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RcList {
    /// The list id.
    pub id: ListId,
    /// The display title.
    pub title: String,
    /// How many incomplete reminders the list holds.
    pub reminder_count: u32,
}

/// A repeat rule as `remindctl` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RcRecurrence {
    /// The frequency, such as `weekly`.
    pub frequency: String,
    /// How many frequency units lie between occurrences.
    pub interval: u32,
}

/// A location trigger as `remindctl` reports it; coordinates and radius are never read.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct RcLocationTrigger {
    /// The geocoded address, absent on a trigger made from coordinates alone.
    pub address: Option<String>,
    /// When the trigger fires.
    pub proximity: Proximity,
}

impl fmt::Debug for RcLocationTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let RcLocationTrigger {
            address: _,
            proximity,
        } = self;
        f.debug_struct("RcLocationTrigger")
            .field("address", &"<redacted>")
            .field("proximity", proximity)
            .finish()
    }
}

/// A reminder as `remindctl` reports it in `show`, `info`, `add` and `edit`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RcReminder {
    /// The reminder id.
    pub id: ReminderId,
    /// The title.
    pub title: String,
    /// The notes, absent when empty.
    pub notes: Option<String>,
    /// Whether the reminder is completed.
    pub is_completed: bool,
    /// When the reminder was completed.
    pub completion_date: Option<RcInstant>,
    /// The id of the list holding the reminder.
    #[serde(rename = "listID")]
    pub list_id: ListId,
    /// The title of the list holding the reminder.
    pub list_name: String,
    /// The priority.
    pub priority: Priority,
    /// The due instant; local midnight of the day for an all-day reminder.
    pub due_date: Option<RcInstant>,
    /// Whether the due date is a whole day.
    #[serde(default)]
    pub due_date_is_all_day: bool,
    /// The repeat rule.
    pub recurrence_rule: Option<RcRecurrence>,
    /// The location trigger.
    pub location_trigger: Option<RcLocationTrigger>,
}

/// The output of `remindctl delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct RcDeleted {
    /// How many reminders were deleted.
    pub deleted: u32,
}

/// The output of `remindctl status`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RcStatus {
    /// Whether the app may read and write reminders.
    pub authorized: bool,
    /// The authorization status, such as `full-access`.
    pub status: String,
}

/// A reminder list as the bridge returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct List {
    /// The list id.
    pub id: ListId,
    /// The display title.
    pub title: String,
    /// How many incomplete reminders the list holds.
    pub open: u32,
    /// True for a write list.
    pub writable: bool,
}

impl RcList {
    /// Converts to the bridge's list with the access the policy grants.
    pub fn into_list(self, access: Access) -> List {
        let RcList {
            id,
            title,
            reminder_count,
        } = self;
        let writable = match access {
            Access::ReadOnly => false,
            Access::Writable => true,
        };
        List {
            id,
            title,
            open: reminder_count,
            writable,
        }
    }
}

/// The list a reminder belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReminderList {
    /// The list id.
    pub id: ListId,
    /// The list title.
    pub title: String,
}

/// A location trigger as the bridge returns it; addresses and coordinates never leave the bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReminderLocation {
    /// The configured place whose address the trigger has, or `None` for any other trigger.
    pub place: Option<PlaceName>,
    /// When the trigger fires.
    pub proximity: Proximity,
}

/// A reminder as the bridge returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reminder {
    /// The reminder id.
    pub id: ReminderId,
    /// The title.
    pub title: String,
    /// The notes.
    pub notes: Option<String>,
    /// Whether the reminder is completed.
    pub completed: bool,
    /// When the reminder was completed, in the local zone.
    pub completed_at: Option<LocalTime>,
    /// The due date: a local date-time, or a date for an all-day reminder.
    pub due: Option<Due>,
    /// Whether the due date is a whole day.
    pub all_day: bool,
    /// The repeat rule.
    pub repeat: Option<ReminderRepeat>,
    /// The priority.
    pub priority: Priority,
    /// The list holding the reminder.
    pub list: ReminderList,
    /// The location trigger.
    pub location: Option<ReminderLocation>,
}

/// What converting a `remindctl` reminder needs from the bridge.
#[derive(Debug, Clone, Copy)]
pub struct Conversion<'a, Tz> {
    /// The configured places, matched against trigger addresses.
    pub places: &'a [Place],
    /// The zone due dates are reported in.
    pub zone: &'a Tz,
}

impl RcReminder {
    /// Converts to the bridge's reminder, dropping addresses and coordinates.
    pub fn into_reminder<Tz: TimeZone>(self, conversion: Conversion<'_, Tz>) -> Reminder {
        let Conversion { places, zone } = conversion;
        let RcReminder {
            id,
            title,
            notes,
            is_completed,
            completion_date,
            list_id,
            list_name,
            priority,
            due_date,
            due_date_is_all_day,
            recurrence_rule,
            location_trigger,
        } = self;
        let completed_at = completion_date.map(|instant| LocalTime(instant.local(zone)));
        let due = due_date.map(|instant| due(instant, due_date_is_all_day, zone));
        let repeat = recurrence_rule.map(|rule| repeat(&rule));
        let location = location_trigger.map(|trigger| location(trigger, places));
        Reminder {
            id,
            title,
            notes,
            completed: is_completed,
            completed_at,
            due,
            all_day: due_date_is_all_day,
            repeat,
            priority,
            list: ReminderList {
                id: list_id,
                title: list_name,
            },
            location,
        }
    }
}

fn due<Tz: TimeZone>(instant: RcInstant, all_day: bool, zone: &Tz) -> Due {
    let local = instant.local(zone);
    if all_day {
        return Due::Day(local.date_naive());
    }
    Due::At(local)
}

fn repeat(rule: &RcRecurrence) -> ReminderRepeat {
    let RcRecurrence {
        frequency,
        interval,
    } = rule;
    match (frequency.as_str(), interval) {
        ("daily", 1) => ReminderRepeat::Rule(Repeat::Daily),
        ("weekly", 1) => ReminderRepeat::Rule(Repeat::Weekly),
        ("weekly", 2) => ReminderRepeat::Rule(Repeat::Biweekly),
        ("monthly", 1) => ReminderRepeat::Rule(Repeat::Monthly),
        ("yearly", 1) => ReminderRepeat::Rule(Repeat::Yearly),
        (_, _) => ReminderRepeat::Custom,
    }
}

fn location(trigger: RcLocationTrigger, places: &[Place]) -> ReminderLocation {
    let RcLocationTrigger { address, proximity } = trigger;
    let mut place = None;
    if let Some(address) = address {
        for Place {
            name,
            address: configured,
            radius: _,
        } in places
        {
            if configured.as_str() == address {
                place = Some(name.clone());
                break;
            }
        }
    }
    ReminderLocation { place, proximity }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::config::Config;
    use crate::fake_ekctl::fixture_text;

    const WRITE_LIST: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const SHOP_ADDRESS: &str = "1 Example Street, Exampletown";

    fn places() -> Vec<Place> {
        let config = Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\n[places]\nshop = {{ address = \"{SHOP_ADDRESS}\", radius = 150 }}\nhome = {{ address = \"2 Home Lane\" }}\n"
        ))
        .unwrap();
        config.places
    }

    fn plus_two() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).unwrap()
    }

    fn convert(reminder: RcReminder) -> Value {
        let places = places();
        let zone = plus_two();
        let reminder = reminder.into_reminder(Conversion {
            places: &places,
            zone: &zone,
        });
        serde_json::to_value(reminder).unwrap()
    }

    fn reminder(name: &str) -> RcReminder {
        serde_json::from_str(&fixture_text(name)).unwrap()
    }

    fn shown() -> Vec<RcReminder> {
        serde_json::from_str(&fixture_text("remindctl_show.json")).unwrap()
    }

    #[test]
    fn list_fixture_parses() {
        let lists: Vec<RcList> =
            serde_json::from_str(&fixture_text("remindctl_list.json")).unwrap();
        assert_eq!(lists.len(), 3);
        let list = lists[0].clone().into_list(Access::Writable);
        assert_eq!(
            serde_json::to_value(list).unwrap(),
            json!({"id": WRITE_LIST, "title": "Shopping", "open": 4, "writable": true})
        );
        let list = lists[1].clone().into_list(Access::ReadOnly);
        assert!(!list.writable);
        assert_eq!(list.open, 2);
    }

    #[test]
    fn show_fixture_parses() {
        assert_eq!(shown().len(), 5);
    }

    #[test]
    fn plain_reminder() {
        let mut reminders = shown();
        assert_eq!(
            convert(reminders.remove(0)),
            json!({
                "id": "0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D",
                "title": "Milk",
                "notes": null,
                "completed": false,
                "completed_at": null,
                "due": null,
                "all_day": false,
                "repeat": null,
                "priority": "none",
                "list": {"id": WRITE_LIST, "title": "Shopping"},
                "location": null,
            })
        );
    }

    #[test]
    fn timed_due_is_converted_to_the_local_zone() {
        let mut reminders = shown();
        let converted = convert(reminders.remove(1));
        assert_eq!(converted["due"], "2026-10-06T09:00:00+02:00");
        assert_eq!(converted["all_day"], false);
        assert_eq!(converted["repeat"], "monthly");
        assert_eq!(converted["priority"], "high");
        assert_eq!(converted["notes"], "Example notes");
    }

    #[test]
    fn all_day_due_is_the_local_date() {
        let mut reminders = shown();
        let converted = convert(reminders.remove(2));
        assert_eq!(converted["due"], "2026-10-07");
        assert_eq!(converted["all_day"], true);
        assert_eq!(converted["repeat"], "biweekly");
    }

    #[test]
    fn trigger_at_a_configured_address_names_the_place() {
        let mut reminders = shown();
        let converted = convert(reminders.remove(3));
        assert_eq!(
            converted["location"],
            json!({"place": "shop", "proximity": "arriving"})
        );
        assert_eq!(converted["repeat"], "custom");
    }

    #[test]
    fn trigger_elsewhere_has_no_place() {
        let mut reminders = shown();
        let converted = convert(reminders.remove(4));
        assert_eq!(
            converted["location"],
            json!({"place": null, "proximity": "leaving"})
        );
        assert_eq!(converted["completed"], true);
        assert_eq!(converted["completed_at"], "2026-10-04T18:45:10+02:00");
    }

    #[test]
    fn addresses_and_coordinates_are_never_returned() {
        for reminder in shown() {
            let text = serde_json::to_string(&convert(reminder)).unwrap();
            assert!(!text.contains("Example Street"), "{text}");
            assert!(!text.contains("Elsewhere"), "{text}");
            assert!(!text.contains("50.000"), "{text}");
            assert!(!text.contains("latitude"), "{text}");
            assert!(!text.contains("radius"), "{text}");
        }
    }

    #[test]
    fn debug_hides_trigger_addresses() {
        let reminder = reminder("remindctl_info_location.json");
        let debug = format!("{reminder:?}");
        assert!(!debug.contains("Example Street"), "{debug}");
        assert!(debug.contains("Leaving"), "{debug}");
    }

    #[test]
    fn info_fixtures_parse() {
        let converted = convert(reminder("remindctl_info.json"));
        assert_eq!(converted["repeat"], "weekly");
        let converted = convert(reminder("remindctl_info_location.json"));
        assert_eq!(
            converted["location"],
            json!({"place": "shop", "proximity": "leaving"})
        );
    }

    #[test]
    fn add_and_edit_fixtures_parse() {
        let converted = convert(reminder("remindctl_add.json"));
        assert_eq!(converted["title"], "Eggs");
        assert_eq!(converted["due"], "2026-10-06T09:00:00+02:00");
        let converted = convert(reminder("remindctl_edit.json"));
        assert_eq!(converted["completed"], true);
        assert_eq!(converted["priority"], "low");
    }

    #[test]
    fn delete_and_status_fixtures_parse() {
        let deleted: RcDeleted =
            serde_json::from_str(&fixture_text("remindctl_delete.json")).unwrap();
        assert_eq!(deleted.deleted, 1);
        let status: RcStatus =
            serde_json::from_str(&fixture_text("remindctl_status.json")).unwrap();
        assert!(status.authorized);
        assert_eq!(status.status, "full-access");
    }

    #[test]
    fn repeat_mapping() {
        let cases = [
            ("daily", 1, ReminderRepeat::Rule(Repeat::Daily)),
            ("weekly", 1, ReminderRepeat::Rule(Repeat::Weekly)),
            ("weekly", 2, ReminderRepeat::Rule(Repeat::Biweekly)),
            ("monthly", 1, ReminderRepeat::Rule(Repeat::Monthly)),
            ("yearly", 1, ReminderRepeat::Rule(Repeat::Yearly)),
            ("daily", 2, ReminderRepeat::Custom),
            ("weekly", 3, ReminderRepeat::Custom),
            ("monthly", 6, ReminderRepeat::Custom),
            ("hourly", 1, ReminderRepeat::Custom),
        ];
        for (frequency, interval, expected) in cases {
            let rule = RcRecurrence {
                frequency: frequency.to_owned(),
                interval,
            };
            assert_eq!(repeat(&rule), expected, "{frequency} {interval}");
        }
    }

    #[test]
    fn due_display() {
        let at = DateTime::parse_from_rfc3339("2026-10-06T09:00:00+02:00").unwrap();
        assert_eq!(Due::At(at).to_string(), "2026-10-06T09:00:00+02:00");
        let utc = DateTime::parse_from_rfc3339("2026-10-06T07:00:00Z").unwrap();
        assert_eq!(Due::At(utc).to_string(), "2026-10-06T07:00:00+00:00");
        let day = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        assert_eq!(Due::Day(day).to_string(), "2026-10-06");
    }

    #[test]
    fn reminder_id_must_be_a_full_uuid() {
        assert!(ReminderId::parse("0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D").is_ok());
        assert!(ReminderId::parse("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d").is_ok());
        for bad in [
            "",
            "1",
            "12",
            "0A1B2C3D",
            "0A1B2C3D-4E5F-4A6B-8C7D",
            "0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D-",
            "0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4G",
            "--0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C",
        ] {
            assert_eq!(ReminderId::parse(bad), Err(InvalidReminderId), "{bad:?}");
        }
    }

    #[test]
    fn non_uuid_ids_in_output_are_rejected() {
        let mut value: Value = serde_json::from_str(&fixture_text("remindctl_info.json")).unwrap();
        value["id"] = json!("1");
        assert!(serde_json::from_value::<RcReminder>(value).is_err());
        let mut value: Value = serde_json::from_str(&fixture_text("remindctl_info.json")).unwrap();
        value["listID"] = json!("abc");
        assert!(serde_json::from_value::<RcReminder>(value).is_err());
    }

    #[test]
    fn unknown_priority_is_rejected() {
        let mut value: Value = serde_json::from_str(&fixture_text("remindctl_info.json")).unwrap();
        value["priority"] = json!("urgent");
        assert!(serde_json::from_value::<RcReminder>(value).is_err());
    }

    #[test]
    fn local_time_serializes_with_whole_seconds() {
        let at = DateTime::parse_from_rfc3339("2026-10-06T09:00:00.75+02:00").unwrap();
        assert_eq!(
            serde_json::to_value(LocalTime(at)).unwrap(),
            json!("2026-10-06T09:00:00+02:00")
        );
        assert_eq!(LocalTime(at).as_datetime(), at);
    }
}
