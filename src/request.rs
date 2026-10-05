use chrono::{DateTime, FixedOffset, NaiveTime, TimeDelta, Timelike, Weekday};
use serde::Deserialize;
use url::Url;

use crate::config::CalendarId;
use crate::ekctl::{EventChanges, EventRange, FreeQuery, NewEvent, Weekdays, WorkingHours};
use crate::model::Event;

const MAX_SPAN_DAYS: i64 = 62;
const MAX_TITLE: usize = 500;
const MAX_LOCATION: usize = 500;
const MAX_NOTES: usize = 10_000;
const MAX_URL: usize = 2_000;

/// A request the bridge refuses before running `ekctl`, answered with `400`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Invalid(String);

impl Invalid {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// The body of `POST /v1/events`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    /// The title.
    pub title: String,
    /// The start, RFC 3339.
    pub start: String,
    /// The end, RFC 3339.
    pub end: String,
    /// The location.
    pub location: Option<String>,
    /// The notes.
    pub notes: Option<String>,
    /// The url.
    pub url: Option<String>,
}

/// The body of `PATCH /v1/events/{id}`; absent and `null` fields stay unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateBody {
    /// The new title.
    pub title: Option<String>,
    /// The new start, RFC 3339.
    pub start: Option<String>,
    /// The new end, RFC 3339.
    pub end: Option<String>,
    /// The new location.
    pub location: Option<String>,
    /// The new notes.
    pub notes: Option<String>,
    /// The new url.
    pub url: Option<String>,
}

struct Params {
    single: Vec<(String, String)>,
    calendars: Vec<CalendarId>,
}

impl Params {
    fn parse(raw: Option<&str>, allowed: &[&str]) -> Result<Self, Invalid> {
        let mut params = Self {
            single: Vec::new(),
            calendars: Vec::new(),
        };
        let Some(raw) = raw else {
            return Ok(params);
        };
        for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            if key == "calendar" {
                let Ok(id) = CalendarId::parse(value.into_owned()) else {
                    return Err(Invalid::new("`calendar` must be a calendar id"));
                };
                params.calendars.push(id);
                continue;
            }
            if !allowed.contains(&key.as_ref()) {
                return Err(Invalid::new(format!("unknown query parameter `{key}`")));
            }
            for (seen, _) in &params.single {
                if *seen == key {
                    return Err(Invalid::new(format!("`{key}` is given more than once")));
                }
            }
            params.single.push((key.into_owned(), value.into_owned()));
        }
        Ok(params)
    }

    fn get(&self, key: &str) -> Option<&str> {
        for (name, value) in &self.single {
            if name == key {
                return Some(value);
            }
        }
        None
    }

    fn timestamp(&self, key: &str) -> Result<Option<DateTime<FixedOffset>>, Invalid> {
        let Some(value) = self.get(key) else {
            return Ok(None);
        };
        let value = restore_plus_offset(value);
        Ok(Some(parse_timestamp(key, &value)?))
    }

    fn required_timestamp(&self, key: &str) -> Result<DateTime<FixedOffset>, Invalid> {
        let Some(value) = self.timestamp(key)? else {
            return Err(Invalid::new(format!("`{key}` is required")));
        };
        Ok(value)
    }

    fn number(&self, key: &str, default: u32, min: u32, max: u32) -> Result<u32, Invalid> {
        let Some(value) = self.get(key) else {
            return Ok(default);
        };
        let invalid = || Invalid::new(format!("`{key}` must be an integer from {min} to {max}"));
        let Ok(value) = value.parse::<u32>() else {
            return Err(invalid());
        };
        if value < min || value > max {
            return Err(invalid());
        }
        Ok(value)
    }
}

fn restore_plus_offset(value: &str) -> String {
    let mut value = value.to_owned();
    let Some(position) = value.len().checked_sub(6) else {
        return value;
    };
    if value.is_char_boundary(position) && value[position..].starts_with(' ') {
        value.replace_range(position..position + 1, "+");
    }
    value
}

fn parse_timestamp(key: &str, value: &str) -> Result<DateTime<FixedOffset>, Invalid> {
    let Ok(value) = DateTime::parse_from_rfc3339(value) else {
        return Err(Invalid::new(format!(
            "`{key}` must be an RFC 3339 timestamp"
        )));
    };
    if value.nanosecond() != 0 {
        return Err(Invalid::new(format!(
            "`{key}` must not have fractional seconds"
        )));
    }
    Ok(value)
}

fn check_range(
    from: DateTime<FixedOffset>,
    to: DateTime<FixedOffset>,
    from_key: &str,
    to_key: &str,
) -> Result<(), Invalid> {
    if to <= from {
        return Err(Invalid::new(format!(
            "`{to_key}` must be after `{from_key}`"
        )));
    }
    Ok(())
}

fn check_span(from: DateTime<FixedOffset>, to: DateTime<FixedOffset>) -> Result<(), Invalid> {
    if to - from > TimeDelta::days(MAX_SPAN_DAYS) {
        return Err(Invalid::new(format!(
            "the range must not exceed {MAX_SPAN_DAYS} days"
        )));
    }
    Ok(())
}

/// Parses the query of `GET /v1/events`; `calendars` holds the requested ids, empty for all.
pub fn events_query(raw: Option<&str>) -> Result<EventRange, Invalid> {
    let params = Params::parse(raw, &["from", "to"])?;
    let from = params.required_timestamp("from")?;
    let to = params.required_timestamp("to")?;
    check_range(from, to, "from", "to")?;
    check_span(from, to)?;
    let Params {
        single: _,
        calendars,
    } = params;
    Ok(EventRange {
        calendars,
        from,
        to,
    })
}

/// Parses the query of `GET /v1/free`; `calendars` holds the requested ids, empty for all.
pub fn free_query(raw: Option<&str>) -> Result<FreeQuery, Invalid> {
    let params = Params::parse(
        raw,
        &[
            "duration",
            "working_hours",
            "weekdays",
            "buffer",
            "limit",
            "from",
            "to",
        ],
    )?;
    let duration_minutes = params.number("duration", 30, 5, 1440)?;
    let working_hours = match params.get("working_hours") {
        Some(value) => parse_working_hours(value)?,
        None => WorkingHours::Window {
            start: NaiveTime::from_hms_opt(9, 0, 0).unwrap_or_default(),
            end: NaiveTime::from_hms_opt(17, 0, 0).unwrap_or_default(),
        },
    };
    let weekdays = match params.get("weekdays") {
        Some(value) => parse_weekdays(value)?,
        None => Weekdays::Weekdays,
    };
    let buffer_minutes = params.number("buffer", 0, 0, 240)?;
    let limit = params.number("limit", 20, 1, 100)?;
    let from = params.timestamp("from")?;
    let to = params.timestamp("to")?;
    if let (Some(from), Some(to)) = (from, to) {
        check_range(from, to, "from", "to")?;
        check_span(from, to)?;
    }
    let Params {
        single: _,
        calendars,
    } = params;
    Ok(FreeQuery {
        calendars,
        duration_minutes,
        working_hours,
        weekdays,
        buffer_minutes,
        limit,
        from,
        to,
    })
}

fn parse_working_hours(value: &str) -> Result<WorkingHours, Invalid> {
    if value == "all" {
        return Ok(WorkingHours::All);
    }
    let invalid =
        || Invalid::new("`working_hours` must be `all` or HH:MM-HH:MM with start before end");
    let Some((start, end)) = value.split_once('-') else {
        return Err(invalid());
    };
    let (Some(start), Some(end)) = (parse_clock(start), parse_clock(end)) else {
        return Err(invalid());
    };
    if start >= end {
        return Err(invalid());
    }
    Ok(WorkingHours::Window { start, end })
}

fn parse_clock(value: &str) -> Option<NaiveTime> {
    if value.len() != 5 {
        return None;
    }
    NaiveTime::parse_from_str(value, "%H:%M").ok()
}

fn parse_weekdays(value: &str) -> Result<Weekdays, Invalid> {
    match value {
        "weekdays" => return Ok(Weekdays::Weekdays),
        "weekends" => return Ok(Weekdays::Weekends),
        "all" => return Ok(Weekdays::All),
        _list => {}
    }
    let mut days = Vec::new();
    for item in value.split(',') {
        let item = item.trim().to_lowercase();
        let (first, last) = match item.split_once('-') {
            Some((first, last)) => (parse_day(first)?, parse_day(last)?),
            None => {
                let day = parse_day(&item)?;
                (day, day)
            }
        };
        let first = first.num_days_from_monday();
        let last = last.num_days_from_monday();
        if first > last {
            return Err(Invalid::new(format!(
                "`weekdays` range `{item}` must run from an earlier to a later day"
            )));
        }
        let mut day = Weekday::Mon;
        for index in 0..7 {
            if index >= first && index <= last && !days.contains(&day) {
                days.push(day);
            }
            day = day.succ();
        }
    }
    Ok(Weekdays::Days(days))
}

fn parse_day(name: &str) -> Result<Weekday, Invalid> {
    match name {
        "monday" | "mon" => Ok(Weekday::Mon),
        "tuesday" | "tue" => Ok(Weekday::Tue),
        "wednesday" | "wed" => Ok(Weekday::Wed),
        "thursday" | "thu" => Ok(Weekday::Thu),
        "friday" | "fri" => Ok(Weekday::Fri),
        "saturday" | "sat" => Ok(Weekday::Sat),
        "sunday" | "sun" => Ok(Weekday::Sun),
        unknown => Err(Invalid::new(format!(
            "`weekdays` has an unknown day `{unknown}`"
        ))),
    }
}

fn check_text(key: &str, value: &str, max: usize) -> Result<(), Invalid> {
    if value.chars().count() > max {
        return Err(Invalid::new(format!(
            "`{key}` must be at most {max} characters"
        )));
    }
    for c in value.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            return Err(Invalid::new(format!(
                "`{key}` must not contain control characters"
            )));
        }
    }
    Ok(())
}

fn parse_title(value: String) -> Result<String, Invalid> {
    if value.trim().is_empty() {
        return Err(Invalid::new("`title` must not be blank"));
    }
    check_text("title", &value, MAX_TITLE)?;
    Ok(value)
}

fn parse_optional_text(
    key: &str,
    value: Option<String>,
    max: usize,
) -> Result<Option<String>, Invalid> {
    let Some(value) = value else {
        return Ok(None);
    };
    check_text(key, &value, max)?;
    Ok(Some(value))
}

fn parse_url(value: Option<String>) -> Result<Option<Url>, Invalid> {
    let Some(value) = value else {
        return Ok(None);
    };
    check_text("url", &value, MAX_URL)?;
    let invalid = || Invalid::new("`url` must be an absolute http or https URL");
    let Ok(url) = Url::parse(&value) else {
        return Err(invalid());
    };
    match url.scheme() {
        "http" | "https" => Ok(Some(url)),
        _other => Err(invalid()),
    }
}

fn parse_optional_timestamp(
    key: &str,
    value: Option<String>,
) -> Result<Option<DateTime<FixedOffset>>, Invalid> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(Some(parse_timestamp(key, &value)?))
}

/// Parses and validates a `POST /v1/events` body.
pub fn create_body(body: &[u8]) -> Result<NewEvent, Invalid> {
    let body: CreateBody = parse_json(body)?;
    let CreateBody {
        title,
        start,
        end,
        location,
        notes,
        url,
    } = body;
    let title = parse_title(title)?;
    let start = parse_timestamp("start", &start)?;
    let end = parse_timestamp("end", &end)?;
    check_range(start, end, "start", "end")?;
    Ok(NewEvent {
        title,
        start,
        end,
        location: parse_optional_text("location", location, MAX_LOCATION)?,
        notes: parse_optional_text("notes", notes, MAX_NOTES)?,
        url: parse_url(url)?,
    })
}

/// Parses and validates a `PATCH /v1/events/{id}` body, which must change at least one field.
pub fn update_body(body: &[u8]) -> Result<EventChanges, Invalid> {
    let body: UpdateBody = parse_json(body)?;
    let UpdateBody {
        title,
        start,
        end,
        location,
        notes,
        url,
    } = body;
    let title = match title {
        Some(title) => Some(parse_title(title)?),
        None => None,
    };
    let changes = EventChanges {
        title,
        start: parse_optional_timestamp("start", start)?,
        end: parse_optional_timestamp("end", end)?,
        location: parse_optional_text("location", location, MAX_LOCATION)?,
        notes: parse_optional_text("notes", notes, MAX_NOTES)?,
        url: parse_url(url)?,
    };
    if changes == EventChanges::default() {
        return Err(Invalid::new("the update changes no field"));
    }
    Ok(changes)
}

/// Checks that the event range after `changes` still ends after it starts.
pub fn merged_range(changes: &EventChanges, existing: &Event) -> Result<(), Invalid> {
    let start = match changes.start {
        Some(start) => start,
        None => existing.start.as_datetime(),
    };
    let end = match changes.end {
        Some(end) => end,
        None => existing.end.as_datetime(),
    };
    check_range(start, end, "start", "end")
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Invalid> {
    match serde_json::from_slice(body) {
        Ok(value) => Ok(value),
        Err(err) => Err(Invalid::new(format!("invalid JSON body: {err}"))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::EkEventEnvelope;

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";

    fn at(value: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(value).unwrap()
    }

    fn calendar_id(value: &str) -> CalendarId {
        CalendarId::parse(value.to_owned()).unwrap()
    }

    fn message<T: std::fmt::Debug>(result: Result<T, Invalid>) -> String {
        result.unwrap_err().to_string()
    }

    fn body(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    fn valid_create() -> serde_json::Value {
        json!({
            "title": "Lunch",
            "start": "2026-10-05T12:00:00+02:00",
            "end": "2026-10-05T13:00:00+02:00"
        })
    }

    fn with(mut base: serde_json::Value, key: &str, value: serde_json::Value) -> Vec<u8> {
        base[key] = value;
        body(base)
    }

    fn standup() -> Event {
        let EkEventEnvelope { event } =
            serde_json::from_str(include_str!("../fixtures/show_event.json")).unwrap();
        Event::from(event)
    }

    #[test]
    fn events_query_valid() {
        let request = events_query(Some(&format!(
            "from=2026-10-05T00:00:00%2B02:00&to=2026-10-12T00:00:00Z&calendar={READ_ID}&calendar={WRITE_ID}"
        )))
        .unwrap();
        assert_eq!(
            request,
            EventRange {
                calendars: vec![calendar_id(READ_ID), calendar_id(WRITE_ID)],
                from: at("2026-10-05T00:00:00+02:00"),
                to: at("2026-10-12T00:00:00Z"),
            }
        );
    }

    #[test]
    fn events_query_unencoded_plus_offset() {
        let request = events_query(Some(
            "from=2026-10-05T00:00:00+02:00&to=2026-10-06T00:00:00+02:00",
        ))
        .unwrap();
        assert_eq!(request.from, at("2026-10-05T00:00:00+02:00"));
        assert!(request.calendars.is_empty());
    }

    #[test]
    fn events_query_requires_range() {
        assert_eq!(message(events_query(None)), "`from` is required");
        assert_eq!(
            message(events_query(Some("from=2026-10-05T00:00:00Z"))),
            "`to` is required"
        );
    }

    #[test]
    fn events_query_rejects_bad_timestamps() {
        assert_eq!(
            message(events_query(Some(
                "from=2026-10-05&to=2026-10-06T00:00:00Z"
            ))),
            "`from` must be an RFC 3339 timestamp"
        );
        assert_eq!(
            message(events_query(Some(
                "from=2026-10-05T00:00:00.5Z&to=2026-10-06T00:00:00Z"
            ))),
            "`from` must not have fractional seconds"
        );
        assert!(
            events_query(Some(
                "from=2026-10-05T00:00:00.000Z&to=2026-10-06T00:00:00Z"
            ))
            .is_ok()
        );
    }

    #[test]
    fn events_query_rejects_reversed_and_empty_range() {
        assert_eq!(
            message(events_query(Some(
                "from=2026-10-06T00:00:00Z&to=2026-10-05T00:00:00Z"
            ))),
            "`to` must be after `from`"
        );
        assert!(events_query(Some("from=2026-10-05T00:00:00Z&to=2026-10-05T00:00:00Z")).is_err());
    }

    #[test]
    fn events_query_span_limit() {
        assert!(events_query(Some("from=2026-10-01T00:00:00Z&to=2026-12-02T00:00:00Z")).is_ok());
        assert_eq!(
            message(events_query(Some(
                "from=2026-10-01T00:00:00Z&to=2026-12-02T00:00:01Z"
            ))),
            "the range must not exceed 62 days"
        );
    }

    #[test]
    fn events_query_rejects_unknown_duplicate_and_bad_calendar() {
        assert_eq!(
            message(events_query(Some("calendars=x"))),
            "unknown query parameter `calendars`"
        );
        assert_eq!(
            message(events_query(Some(
                "from=2026-10-05T00:00:00Z&from=2026-10-05T00:00:00Z"
            ))),
            "`from` is given more than once"
        );
        assert_eq!(
            message(events_query(Some("calendar=a,b"))),
            "`calendar` must be a calendar id"
        );
        assert_eq!(
            message(events_query(Some("calendar="))),
            "`calendar` must be a calendar id"
        );
    }

    #[test]
    fn free_query_defaults() {
        assert_eq!(
            free_query(None).unwrap(),
            FreeQuery {
                calendars: Vec::new(),
                duration_minutes: 30,
                working_hours: WorkingHours::Window {
                    start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                    end: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
                },
                weekdays: Weekdays::Weekdays,
                buffer_minutes: 0,
                limit: 20,
                from: None,
                to: None,
            }
        );
    }

    #[test]
    fn free_query_full() {
        let query = free_query(Some(&format!(
            "duration=60&working_hours=all&weekdays=mon-wed,friday,Sun&buffer=15&limit=5&from=2026-10-05T00:00:00Z&to=2026-10-06T00:00:00Z&calendar={READ_ID}"
        )))
        .unwrap();
        assert_eq!(
            query,
            FreeQuery {
                calendars: vec![calendar_id(READ_ID)],
                duration_minutes: 60,
                working_hours: WorkingHours::All,
                weekdays: Weekdays::Days(vec![
                    Weekday::Mon,
                    Weekday::Tue,
                    Weekday::Wed,
                    Weekday::Fri,
                    Weekday::Sun,
                ]),
                buffer_minutes: 15,
                limit: 5,
                from: Some(at("2026-10-05T00:00:00Z")),
                to: Some(at("2026-10-06T00:00:00Z")),
            }
        );
    }

    #[test]
    fn free_query_bounds() {
        assert!(free_query(Some("duration=5&buffer=240&limit=100")).is_ok());
        assert!(free_query(Some("duration=1440&buffer=0&limit=1")).is_ok());
        assert_eq!(
            message(free_query(Some("duration=4"))),
            "`duration` must be an integer from 5 to 1440"
        );
        assert!(free_query(Some("duration=1441")).is_err());
        assert!(free_query(Some("duration=abc")).is_err());
        assert!(free_query(Some("duration=-5")).is_err());
        assert_eq!(
            message(free_query(Some("buffer=241"))),
            "`buffer` must be an integer from 0 to 240"
        );
        assert_eq!(
            message(free_query(Some("limit=0"))),
            "`limit` must be an integer from 1 to 100"
        );
        assert!(free_query(Some("limit=101")).is_err());
    }

    #[test]
    fn free_query_working_hours() {
        let query = free_query(Some("working_hours=08:30-18:15")).unwrap();
        assert_eq!(
            query.working_hours,
            WorkingHours::Window {
                start: NaiveTime::from_hms_opt(8, 30, 0).unwrap(),
                end: NaiveTime::from_hms_opt(18, 15, 0).unwrap(),
            }
        );
        for bad in [
            "17:00-09:00",
            "09:00-09:00",
            "9:00-17:00",
            "09:00",
            "25:00-26:00",
            "none",
        ] {
            assert_eq!(
                message(free_query(Some(&format!("working_hours={bad}")))),
                "`working_hours` must be `all` or HH:MM-HH:MM with start before end",
                "{bad}"
            );
        }
    }

    #[test]
    fn free_query_weekday_keywords() {
        assert_eq!(
            free_query(Some("weekdays=weekends")).unwrap().weekdays,
            Weekdays::Weekends
        );
        assert_eq!(
            free_query(Some("weekdays=all")).unwrap().weekdays,
            Weekdays::All
        );
        assert_eq!(
            free_query(Some("weekdays=weekdays")).unwrap().weekdays,
            Weekdays::Weekdays
        );
    }

    #[test]
    fn free_query_bad_weekdays() {
        assert_eq!(
            message(free_query(Some("weekdays=funday"))),
            "`weekdays` has an unknown day `funday`"
        );
        assert_eq!(
            message(free_query(Some("weekdays=fri-mon"))),
            "`weekdays` range `fri-mon` must run from an earlier to a later day"
        );
        assert!(free_query(Some("weekdays=mon,,tue")).is_err());
        assert!(free_query(Some("weekdays=")).is_err());
    }

    #[test]
    fn free_query_range() {
        assert!(free_query(Some("from=2026-10-05T00:00:00Z")).is_ok());
        assert!(free_query(Some("to=2026-10-05T00:00:00Z")).is_ok());
        assert_eq!(
            message(free_query(Some(
                "from=2026-10-06T00:00:00Z&to=2026-10-05T00:00:00Z"
            ))),
            "`to` must be after `from`"
        );
        assert_eq!(
            message(free_query(Some(
                "from=2026-10-01T00:00:00Z&to=2026-12-03T00:00:00Z"
            ))),
            "the range must not exceed 62 days"
        );
        assert_eq!(
            message(free_query(Some("from=tomorrow"))),
            "`from` must be an RFC 3339 timestamp"
        );
        assert_eq!(
            message(free_query(Some("start=2026-10-05T00:00:00Z"))),
            "unknown query parameter `start`"
        );
    }

    #[test]
    fn create_minimal_and_full() {
        let event = create_body(&body(valid_create())).unwrap();
        assert_eq!(
            event,
            NewEvent {
                title: "Lunch".to_owned(),
                start: at("2026-10-05T12:00:00+02:00"),
                end: at("2026-10-05T13:00:00+02:00"),
                location: None,
                notes: None,
                url: None,
            }
        );
        let event = create_body(&body(json!({
            "title": "-Lunch",
            "start": "2026-10-05T12:00:00+02:00",
            "end": "2026-10-05T13:00:00+02:00",
            "location": "Cafe",
            "notes": "line\n\tindented",
            "url": "https://example.com/menu"
        })))
        .unwrap();
        assert_eq!(event.location.as_deref(), Some("Cafe"));
        assert_eq!(event.notes.as_deref(), Some("line\n\tindented"));
        assert_eq!(event.url.unwrap().as_str(), "https://example.com/menu");
    }

    #[test]
    fn create_title_rules() {
        assert_eq!(
            message(create_body(&with(valid_create(), "title", json!(" \n ")))),
            "`title` must not be blank"
        );
        assert!(create_body(&with(valid_create(), "title", json!("é".repeat(500)))).is_ok());
        assert_eq!(
            message(create_body(&with(
                valid_create(),
                "title",
                json!("a".repeat(501))
            ))),
            "`title` must be at most 500 characters"
        );
        assert_eq!(
            message(create_body(&with(valid_create(), "title", json!("a\rb")))),
            "`title` must not contain control characters"
        );
    }

    #[test]
    fn create_range_rules() {
        assert_eq!(
            message(create_body(&with(
                valid_create(),
                "end",
                json!("2026-10-05T12:00:00+02:00")
            ))),
            "`end` must be after `start`"
        );
        assert_eq!(
            message(create_body(&with(
                valid_create(),
                "start",
                json!("2026-10-05")
            ))),
            "`start` must be an RFC 3339 timestamp"
        );
        assert_eq!(
            message(create_body(&with(
                valid_create(),
                "end",
                json!("2026-10-05T13:00:00.25+02:00")
            ))),
            "`end` must not have fractional seconds"
        );
    }

    #[test]
    fn create_text_limits() {
        assert!(create_body(&with(valid_create(), "location", json!("a".repeat(500)))).is_ok());
        assert_eq!(
            message(create_body(&with(
                valid_create(),
                "location",
                json!("a".repeat(501))
            ))),
            "`location` must be at most 500 characters"
        );
        assert!(create_body(&with(valid_create(), "notes", json!("a".repeat(10_000)))).is_ok());
        assert_eq!(
            message(create_body(&with(
                valid_create(),
                "notes",
                json!("a".repeat(10_001))
            ))),
            "`notes` must be at most 10000 characters"
        );
        assert_eq!(
            message(create_body(&with(
                valid_create(),
                "notes",
                json!("a\u{0}b")
            ))),
            "`notes` must not contain control characters"
        );
        assert_eq!(
            message(create_body(&with(
                valid_create(),
                "location",
                json!("a\u{1b}[31m")
            ))),
            "`location` must not contain control characters"
        );
    }

    #[test]
    fn create_url_rules() {
        for bad in [
            "example.com",
            "/relative",
            "ftp://example.com",
            "javascript:alert(1)",
        ] {
            assert_eq!(
                message(create_body(&with(valid_create(), "url", json!(bad)))),
                "`url` must be an absolute http or https URL",
                "{bad}"
            );
        }
        assert!(create_body(&with(valid_create(), "url", json!("http://example.com"))).is_ok());
        let long = format!("https://example.com/{}", "a".repeat(2_000));
        assert_eq!(
            message(create_body(&with(valid_create(), "url", json!(long)))),
            "`url` must be at most 2000 characters"
        );
    }

    #[test]
    fn create_body_shape() {
        assert!(message(create_body(b"not json")).starts_with("invalid JSON body: "));
        assert!(
            message(create_body(&body(
                json!({"title": "x", "start": "2026-10-05T12:00:00Z"})
            )))
            .contains("missing field `end`")
        );
        assert!(
            message(create_body(&with(valid_create(), "all_day", json!(true))))
                .contains("unknown field `all_day`")
        );
    }

    #[test]
    fn update_partial() {
        let changes = update_body(&body(json!({"start": "2026-10-05T12:00:00+02:00"}))).unwrap();
        assert_eq!(
            changes,
            EventChanges {
                start: Some(at("2026-10-05T12:00:00+02:00")),
                ..EventChanges::default()
            }
        );
    }

    #[test]
    fn update_empty_is_rejected() {
        assert_eq!(message(update_body(b"{}")), "the update changes no field");
        assert_eq!(
            message(update_body(&body(json!({"title": null})))),
            "the update changes no field"
        );
    }

    #[test]
    fn update_validates_fields() {
        assert_eq!(
            message(update_body(&body(json!({"title": ""})))),
            "`title` must not be blank"
        );
        assert_eq!(
            message(update_body(&body(json!({"url": "mailto:a@example.com"})))),
            "`url` must be an absolute http or https URL"
        );
        assert!(message(update_body(&body(json!({"color": "red"})))).contains("unknown field"));
    }

    #[test]
    fn merged_range_uses_existing_end() {
        let existing = standup();
        let ok = EventChanges {
            start: Some(at("2026-10-05T11:15:00+02:00")),
            ..EventChanges::default()
        };
        assert!(merged_range(&ok, &existing).is_ok());
        let late = EventChanges {
            start: Some(at("2026-10-05T11:30:00+02:00")),
            ..EventChanges::default()
        };
        assert_eq!(
            message(merged_range(&late, &existing)),
            "`end` must be after `start`"
        );
        let early_end = EventChanges {
            end: Some(at("2026-10-05T09:00:00Z")),
            ..EventChanges::default()
        };
        assert_eq!(
            message(merged_range(&early_end, &existing)),
            "`end` must be after `start`"
        );
    }

    #[test]
    fn plus_offset_restored_only_at_offset_position() {
        assert_eq!(
            restore_plus_offset("2026-10-05T00:00:00 02:00"),
            "2026-10-05T00:00:00+02:00"
        );
        assert_eq!(
            restore_plus_offset("2026-10-05T00:00:00Z"),
            "2026-10-05T00:00:00Z"
        );
        assert_eq!(restore_plus_offset("x"), "x");
        assert_eq!(restore_plus_offset("ééé"), "ééé");
    }
}
