use std::fmt;

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::config::{CalendarId, has_control_character};

/// An EventKit event identifier, such as `46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct EventId(String);

/// Why a string was rejected as an event id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidEventId {
    /// The id is empty.
    #[error("event id is empty")]
    Empty,
    /// The id contains a control character.
    #[error("event id contains a control character")]
    ControlCharacter,
}

impl EventId {
    /// Accepts a client-supplied id that is non-empty and free of control characters.
    pub fn parse(value: &str) -> Result<Self, InvalidEventId> {
        if value.is_empty() {
            return Err(InvalidEventId::Empty);
        }
        if has_control_character(value) {
            return Err(InvalidEventId::ControlCharacter);
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An RFC 3339 timestamp that keeps the exact text, and so the offset, `ekctl` returned.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp {
    text: String,
    value: DateTime<FixedOffset>,
}

impl Timestamp {
    /// The instant with the offset it was written in.
    pub fn as_datetime(&self) -> DateTime<FixedOffset> {
        self.value
    }

    /// The timestamp as `ekctl` wrote it.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl TryFrom<String> for Timestamp {
    type Error = chrono::ParseError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let value = DateTime::parse_from_rfc3339(&text)?;
        Ok(Self { text, value })
    }
}

impl From<Timestamp> for String {
    fn from(timestamp: Timestamp) -> Self {
        timestamp.text
    }
}

/// What a calendar reported by `ekctl` holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarKind {
    /// An event calendar.
    Event,
    /// A reminder list.
    Reminder,
    /// A kind this bridge does not know.
    #[default]
    #[serde(other)]
    Other,
}

/// A calendar as `ekctl list calendars` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EkCalendar {
    /// The calendar id.
    pub id: CalendarId,
    /// The display title.
    pub title: Option<String>,
    /// The account the calendar syncs through.
    pub source: Option<String>,
    /// The display colour, as `#RRGGBB`.
    pub color: Option<String>,
    /// Whether this is an event calendar or a reminder list.
    #[serde(rename = "type", default)]
    pub kind: CalendarKind,
}

/// The output of `ekctl list calendars`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EkCalendarList {
    /// Every calendar and reminder list.
    pub calendars: Vec<EkCalendar>,
}

/// The calendar reference embedded in an `ekctl` event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct EventCalendar {
    /// The calendar id.
    pub id: CalendarId,
    /// The calendar title.
    pub title: Option<String>,
}

/// An attendee as `ekctl` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EkAttendee {
    /// The display name.
    pub name: Option<String>,
    /// The email address.
    pub email: Option<String>,
    /// The role, such as `required`.
    pub role: Option<String>,
    /// The participation status, such as `accepted`.
    pub status: Option<String>,
}

/// An event as `ekctl` reports it in `list events` and `show event`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EkEvent {
    /// The event id, shared by every occurrence of a recurring event.
    pub id: EventId,
    /// The title.
    pub title: Option<String>,
    /// The start, with `ekctl`'s offset.
    pub start_date: Timestamp,
    /// The end, with `ekctl`'s offset.
    pub end_date: Timestamp,
    /// Whether the event is all-day.
    pub all_day: Option<bool>,
    /// The calendar holding the event.
    pub calendar: EventCalendar,
    /// The location.
    pub location: Option<String>,
    /// The url, present only on some events.
    pub url: Option<String>,
    /// The notes.
    pub notes: Option<String>,
    /// The availability, such as `busy`.
    pub availability: Option<String>,
    /// Whether the event belongs to a recurring series.
    pub has_recurrence_rules: Option<bool>,
    /// The attendees.
    pub attendees: Option<Vec<EkAttendee>>,
}

/// The output of `ekctl list events`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EkEventList {
    /// The events in `ekctl`'s order.
    pub events: Vec<EkEvent>,
}

/// The output of `ekctl show event`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EkEventEnvelope {
    /// The event.
    pub event: EkEvent,
}

/// A free slot as `ekctl free` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EkSlot {
    /// The slot start.
    pub start_date: Timestamp,
    /// The slot end.
    pub end_date: Timestamp,
    /// The slot length in minutes.
    pub duration_minutes: Option<u32>,
    /// The weekday name, such as `monday`.
    pub weekday: Option<String>,
}

/// The output of `ekctl free`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EkFree {
    /// The free slots.
    pub slots: Vec<EkSlot>,
    /// The start of the searched range.
    pub searched_from: Timestamp,
    /// The end of the searched range.
    pub searched_to: Timestamp,
}

/// The event id inside the output of `ekctl add event` and `ekctl update event`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EkWrittenEvent {
    /// The id of the created or updated event.
    pub id: EventId,
}

/// The output of `ekctl add event` and `ekctl update event`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EkWritten {
    /// The written event, with fewer fields than `show event` returns.
    pub event: EkWrittenEvent,
}

/// The only status a successful `ekctl` write reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EkSuccess {
    /// `"success"`.
    Success,
}

/// The output of `ekctl delete event`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EkDeleted {
    /// Always `success`; errors are handled before this shape is parsed.
    pub status: EkSuccess,
}

/// Whether the bridge lets clients write to a calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads only.
    ReadOnly,
    /// The write calendar.
    Writable,
}

/// A calendar as the bridge returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Calendar {
    /// The calendar id.
    pub id: CalendarId,
    /// The display title.
    pub title: Option<String>,
    /// The account the calendar syncs through.
    pub source: Option<String>,
    /// The display colour.
    pub color: Option<String>,
    /// True for the write calendar alone.
    pub writable: bool,
}

impl EkCalendar {
    /// Converts to the bridge's calendar with the access the policy grants.
    pub fn into_calendar(self, access: Access) -> Calendar {
        let EkCalendar {
            id,
            title,
            source,
            color,
            kind: _,
        } = self;
        let writable = match access {
            Access::ReadOnly => false,
            Access::Writable => true,
        };
        Calendar {
            id,
            title,
            source,
            color,
            writable,
        }
    }
}

/// An attendee as the bridge returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attendee {
    /// The display name.
    pub name: Option<String>,
    /// The email address.
    pub email: Option<String>,
    /// The role.
    pub role: Option<String>,
    /// The participation status.
    pub status: Option<String>,
}

impl From<EkAttendee> for Attendee {
    fn from(attendee: EkAttendee) -> Self {
        let EkAttendee {
            name,
            email,
            role,
            status,
        } = attendee;
        Self {
            name,
            email,
            role,
            status,
        }
    }
}

/// An event as the bridge returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    /// The event id.
    pub id: EventId,
    /// The title.
    pub title: Option<String>,
    /// The start, with the offset `ekctl` returned.
    pub start: Timestamp,
    /// The end, with the offset `ekctl` returned.
    pub end: Timestamp,
    /// Whether the event is all-day.
    pub all_day: Option<bool>,
    /// The calendar holding the event.
    pub calendar: EventCalendar,
    /// The location.
    pub location: Option<String>,
    /// The url.
    pub url: Option<String>,
    /// The notes.
    pub notes: Option<String>,
    /// The availability.
    pub availability: Option<String>,
    /// Whether the event belongs to a recurring series.
    pub recurring: Option<bool>,
    /// The attendees, empty when `ekctl` lists none.
    pub attendees: Vec<Attendee>,
}

impl From<EkEvent> for Event {
    fn from(event: EkEvent) -> Self {
        let EkEvent {
            id,
            title,
            start_date,
            end_date,
            all_day,
            calendar,
            location,
            url,
            notes,
            availability,
            has_recurrence_rules,
            attendees,
        } = event;
        let mut converted = Vec::new();
        for attendee in attendees.unwrap_or_default() {
            converted.push(Attendee::from(attendee));
        }
        Self {
            id,
            title,
            start: start_date,
            end: end_date,
            all_day,
            calendar,
            location,
            url,
            notes,
            availability,
            recurring: has_recurrence_rules,
            attendees: converted,
        }
    }
}

/// A free slot as the bridge returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Slot {
    /// The slot start.
    pub start: Timestamp,
    /// The slot end.
    pub end: Timestamp,
    /// The slot length in minutes.
    pub duration_minutes: Option<u32>,
    /// The weekday name.
    pub weekday: Option<String>,
}

impl From<EkSlot> for Slot {
    fn from(slot: EkSlot) -> Self {
        let EkSlot {
            start_date,
            end_date,
            duration_minutes,
            weekday,
        } = slot;
        Self {
            start: start_date,
            end: end_date,
            duration_minutes,
            weekday,
        }
    }
}

/// The free-slot search result as the bridge returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FreeSlots {
    /// The free slots.
    pub slots: Vec<Slot>,
    /// The start of the searched range.
    pub searched_from: Timestamp,
    /// The end of the searched range.
    pub searched_to: Timestamp,
}

impl From<EkFree> for FreeSlots {
    fn from(free: EkFree) -> Self {
        let EkFree {
            slots,
            searched_from,
            searched_to,
        } = free;
        let mut converted = Vec::new();
        for slot in slots {
            converted.push(Slot::from(slot));
        }
        Self {
            slots: converted,
            searched_from,
            searched_to,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const REMINDERS_ID: &str = "2F8BCC68-AD77-B8A4-9218-37BF6271D47D";
    const EVENT_ID: &str = "46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076";

    fn calendar_id(value: &str) -> CalendarId {
        CalendarId::parse(value.to_owned()).unwrap()
    }

    fn timestamp(value: &str) -> Timestamp {
        Timestamp::try_from(value.to_owned()).unwrap()
    }

    fn standup_json() -> serde_json::Value {
        json!({
            "id": EVENT_ID,
            "title": "Standup",
            "start": "2026-10-05T11:00:00+02:00",
            "end": "2026-10-05T11:30:00+02:00",
            "all_day": false,
            "calendar": {"id": READ_ID, "title": "Calendar"},
            "location": "Teams",
            "url": null,
            "notes": "long text",
            "availability": "busy",
            "recurring": true,
            "attendees": [
                {"name": "A Person", "email": "a@example.com", "role": "required", "status": "accepted"}
            ]
        })
    }

    #[test]
    fn list_calendars_fixture() {
        let EkCalendarList { calendars } =
            serde_json::from_str(include_str!("../fixtures/list_calendars.json")).unwrap();
        assert_eq!(
            calendars,
            vec![
                EkCalendar {
                    id: calendar_id(READ_ID),
                    title: Some("Calendar".to_owned()),
                    source: Some("work@example.com".to_owned()),
                    color: Some("#0088FF".to_owned()),
                    kind: CalendarKind::Event,
                },
                EkCalendar {
                    id: calendar_id(WRITE_ID),
                    title: Some("Agent".to_owned()),
                    source: Some("iCloud".to_owned()),
                    color: Some("#34C759".to_owned()),
                    kind: CalendarKind::Event,
                },
                EkCalendar {
                    id: calendar_id(REMINDERS_ID),
                    title: Some("Reminders".to_owned()),
                    source: Some("iCloud".to_owned()),
                    color: Some("#007AFF".to_owned()),
                    kind: CalendarKind::Reminder,
                },
            ]
        );
    }

    #[test]
    fn calendar_converts_with_access() {
        let EkCalendarList { calendars } =
            serde_json::from_str(include_str!("../fixtures/list_calendars.json")).unwrap();
        let mut calendars = calendars.into_iter();
        let read = calendars.next().unwrap().into_calendar(Access::ReadOnly);
        let write = calendars.next().unwrap().into_calendar(Access::Writable);
        assert_eq!(
            serde_json::to_value(read).unwrap(),
            json!({"id": READ_ID, "title": "Calendar", "source": "work@example.com", "color": "#0088FF", "writable": false})
        );
        assert_eq!(
            serde_json::to_value(write).unwrap(),
            json!({"id": WRITE_ID, "title": "Agent", "source": "iCloud", "color": "#34C759", "writable": true})
        );
    }

    #[test]
    fn unknown_or_missing_calendar_kind() {
        let EkCalendarList { calendars } = serde_json::from_value(json!({
            "calendars": [
                {"id": "A", "type": "birthday", "title": null},
                {"id": "B"}
            ]
        }))
        .unwrap();
        assert_eq!(calendars[0].kind, CalendarKind::Other);
        assert_eq!(calendars[0].title, None);
        assert_eq!(calendars[1].kind, CalendarKind::Other);
    }

    #[test]
    fn list_events_fixture_converts() {
        let EkEventList { events } =
            serde_json::from_str(include_str!("../fixtures/list_events.json")).unwrap();
        assert_eq!(events.len(), 1);
        let event = Event::from(events.into_iter().next().unwrap());
        assert_eq!(serde_json::to_value(&event).unwrap(), standup_json());
    }

    #[test]
    fn show_event_fixture_converts() {
        let EkEventEnvelope { event } =
            serde_json::from_str(include_str!("../fixtures/show_event.json")).unwrap();
        let event = Event::from(event);
        assert_eq!(event.id.as_str(), EVENT_ID);
        assert_eq!(event.calendar.id, calendar_id(READ_ID));
        assert_eq!(serde_json::to_value(&event).unwrap(), standup_json());
    }

    #[test]
    fn sparse_event_becomes_nulls() {
        let event: EkEvent = serde_json::from_value(json!({
            "id": "X:1",
            "title": null,
            "startDate": "2026-10-05T00:00:00Z",
            "endDate": "2026-10-06T00:00:00Z",
            "calendar": {"id": READ_ID},
            "attendees": null,
            "somethingNew": [1, 2, 3]
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(Event::from(event)).unwrap(),
            json!({
                "id": "X:1",
                "title": null,
                "start": "2026-10-05T00:00:00Z",
                "end": "2026-10-06T00:00:00Z",
                "all_day": null,
                "calendar": {"id": READ_ID, "title": null},
                "location": null,
                "url": null,
                "notes": null,
                "availability": null,
                "recurring": null,
                "attendees": []
            })
        );
    }

    #[test]
    fn event_with_url() {
        let event: EkEvent = serde_json::from_value(json!({
            "id": "X:1",
            "title": "Call",
            "startDate": "2026-10-05T10:00:00+02:00",
            "endDate": "2026-10-05T11:00:00+02:00",
            "calendar": {"id": READ_ID, "title": "Calendar"},
            "url": "https://example.com/call"
        }))
        .unwrap();
        assert_eq!(
            Event::from(event).url.as_deref(),
            Some("https://example.com/call")
        );
    }

    #[test]
    fn event_with_bad_timestamp_is_rejected() {
        let result = serde_json::from_value::<EkEvent>(json!({
            "id": "X:1",
            "startDate": "tomorrow",
            "endDate": "2026-10-05T11:00:00+02:00",
            "calendar": {"id": READ_ID}
        }));
        assert!(result.is_err());
    }

    #[test]
    fn event_without_calendar_is_rejected() {
        let result = serde_json::from_value::<EkEvent>(json!({
            "id": "X:1",
            "startDate": "2026-10-05T10:00:00+02:00",
            "endDate": "2026-10-05T11:00:00+02:00"
        }));
        assert!(result.is_err());
    }

    #[test]
    fn free_fixture_converts() {
        let free: EkFree = serde_json::from_str(include_str!("../fixtures/free.json")).unwrap();
        assert_eq!(
            serde_json::to_value(FreeSlots::from(free)).unwrap(),
            json!({
                "slots": [{
                    "start": "2026-10-05T09:00:00+02:00",
                    "end": "2026-10-05T10:00:00+02:00",
                    "duration_minutes": 60,
                    "weekday": "monday"
                }],
                "searched_from": "2026-10-04T21:40:21+02:00",
                "searched_to": "2026-10-07T21:40:21+02:00"
            })
        );
    }

    #[test]
    fn add_event_fixture_parses() {
        let EkWritten { event } =
            serde_json::from_str(include_str!("../fixtures/add_event.json")).unwrap();
        assert_eq!(event.id.as_str(), "NEW123:EVENT456");
    }

    #[test]
    fn delete_event_fixture_parses() {
        let EkDeleted { status } =
            serde_json::from_str(include_str!("../fixtures/delete_event.json")).unwrap();
        assert_eq!(status, EkSuccess::Success);
    }

    #[test]
    fn delete_shape_needs_success() {
        assert!(serde_json::from_value::<EkDeleted>(json!({"status": "pending"})).is_err());
        assert!(serde_json::from_value::<EkDeleted>(json!({})).is_err());
    }

    #[test]
    fn timestamp_keeps_text_and_offset() {
        let utc = timestamp("2026-02-10T12:30:00Z");
        assert_eq!(utc.as_str(), "2026-02-10T12:30:00Z");
        assert_eq!(
            serde_json::to_value(&utc).unwrap(),
            json!("2026-02-10T12:30:00Z")
        );
        let berlin = timestamp("2026-02-10T13:30:00+01:00");
        assert_eq!(berlin.as_datetime(), utc.as_datetime());
        assert_eq!(berlin.as_datetime().offset().local_minus_utc(), 3600);
    }

    #[test]
    fn timestamp_rejects_non_rfc3339() {
        assert!(Timestamp::try_from("2026-02-10 12:30".to_owned()).is_err());
        assert!(Timestamp::try_from("2026-02-10T12:30:00".to_owned()).is_err());
    }

    #[test]
    fn event_id_parse() {
        assert_eq!(EventId::parse(EVENT_ID).unwrap().as_str(), EVENT_ID);
        assert_eq!(EventId::parse(""), Err(InvalidEventId::Empty));
        assert_eq!(
            EventId::parse("abc\ndef"),
            Err(InvalidEventId::ControlCharacter)
        );
        assert_eq!(EventId::parse("-rf").unwrap().to_string(), "-rf");
    }
}
