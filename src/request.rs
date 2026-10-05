use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeDelta, Timelike, Weekday};
use serde::{Deserialize, Deserializer};
use url::Url;

use crate::config::{AccountName, CalendarId, ListId, Place};
use crate::ekctl::{EventChanges, EventRange, FreeQuery, NewEvent, Weekdays, WorkingHours};
use crate::mail::MessageId;
use crate::mail::store::{Cursor, CursorError, DEFAULT_LIMIT, MAX_LIMIT, MessageQuery, ReadFilter};
use crate::model::Event;
use crate::remindctl::{Change, Completion, NewReminder, ReminderChanges, ShowFilter, Trigger};
use crate::reminders_model::{Due, Priority, Proximity, RcReminder, Repeat};

const MAX_SPAN_DAYS: i64 = 62;
const MAX_TITLE: usize = 500;
const MAX_LOCATION: usize = 500;
const MAX_NOTES: usize = 10_000;
const MAX_URL: usize = 2_000;
const MAX_MAILBOX: usize = 1_000;
const MAX_QUERY: usize = 500;

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
    /// The calendar to create the event in; must be a write calendar.
    pub calendar: String,
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
            params.insert((key.into_owned(), value.into_owned()), allowed)?;
        }
        Ok(params)
    }

    fn insert(&mut self, pair: (String, String), allowed: &[&str]) -> Result<(), Invalid> {
        let (key, value) = pair;
        if !allowed.contains(&key.as_str()) {
            return Err(Invalid::new(format!("unknown query parameter `{key}`")));
        }
        for (seen, _) in &self.single {
            if *seen == key {
                return Err(Invalid::new(format!("`{key}` is given more than once")));
            }
        }
        self.single.push((key, value));
        Ok(())
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

    fn number(&self, key: &str, bounds: Bounds) -> Result<u32, Invalid> {
        let Bounds { default, min, max } = bounds;
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

#[derive(Debug, Clone, Copy)]
struct Bounds {
    default: u32,
    min: u32,
    max: u32,
}

const DURATION_BOUNDS: Bounds = Bounds {
    default: 30,
    min: 5,
    max: 1440,
};
const BUFFER_BOUNDS: Bounds = Bounds {
    default: 0,
    min: 0,
    max: 240,
};
const LIMIT_BOUNDS: Bounds = Bounds {
    default: 20,
    min: 1,
    max: 100,
};
const MAIL_LIMIT_BOUNDS: Bounds = Bounds {
    default: DEFAULT_LIMIT,
    min: 1,
    max: MAX_LIMIT,
};

#[derive(Debug, Clone, Copy)]
struct RangeKeys {
    from: &'static str,
    to: &'static str,
}

const FROM_TO: RangeKeys = RangeKeys {
    from: "from",
    to: "to",
};
const START_END: RangeKeys = RangeKeys {
    from: "start",
    to: "end",
};
const SINCE_UNTIL: RangeKeys = RangeKeys {
    from: "since",
    to: "until",
};

fn check_range(
    keys: RangeKeys,
    from: DateTime<FixedOffset>,
    to: DateTime<FixedOffset>,
) -> Result<(), Invalid> {
    let RangeKeys {
        from: from_key,
        to: to_key,
    } = keys;
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
    check_range(FROM_TO, from, to)?;
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
    let duration_minutes = params.number("duration", DURATION_BOUNDS)?;
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
    let buffer_minutes = params.number("buffer", BUFFER_BOUNDS)?;
    let limit = params.number("limit", LIMIT_BOUNDS)?;
    let from = params.timestamp("from")?;
    let to = params.timestamp("to")?;
    match (from, to) {
        (Some(from), Some(to)) => {
            check_range(FROM_TO, from, to)?;
            check_span(from, to)?;
        }
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => {
            return Err(Invalid::new("`from` and `to` must be given together"));
        }
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
    let value = value.to_lowercase();
    match value.as_str() {
        "weekdays" => return Ok(Weekdays::Weekdays),
        "weekends" => return Ok(Weekdays::Weekends),
        "all" => return Ok(Weekdays::All),
        _list => {}
    }
    let mut days = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        let (first, last) = match item.split_once('-') {
            Some((first, last)) => (parse_day(first)?, parse_day(last)?),
            None => {
                let day = parse_day(item)?;
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
pub fn create_body(body: &[u8]) -> Result<CreateRequest, Invalid> {
    let body: CreateBody = parse_json(body)?;
    let CreateBody {
        calendar,
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
    check_range(START_END, start, end)?;
    let Ok(calendar) = CalendarId::parse(calendar) else {
        return Err(Invalid::new("`calendar` must be a calendar id".to_owned()));
    };
    Ok(CreateRequest {
        calendar,
        event: NewEvent {
            title,
            start,
            end,
            location: parse_optional_text("location", location, MAX_LOCATION)?,
            notes: parse_optional_text("notes", notes, MAX_NOTES)?,
            url: parse_url(url)?,
        },
    })
}

/// A validated `POST /v1/events` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    /// The calendar to create the event in.
    pub calendar: CalendarId,
    /// The event to create.
    pub event: NewEvent,
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
    check_range(START_END, start, end)
}

/// The query of `GET /v1/reminders`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemindersQuery {
    /// Which reminders to return.
    pub status: ShowFilter,
    /// The requested lists, empty for every readable list.
    pub lists: Vec<ListId>,
}

/// Parses the query of `GET /v1/reminders`.
pub fn reminders_query(raw: Option<&str>) -> Result<RemindersQuery, Invalid> {
    let mut query = RemindersQuery {
        status: ShowFilter::Open,
        lists: Vec::new(),
    };
    let Some(raw) = raw else {
        return Ok(query);
    };
    let mut status_seen = false;
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        match key.as_ref() {
            "list" => {
                let Ok(id) = ListId::parse(value.into_owned()) else {
                    return Err(Invalid::new("`list` must be a list id"));
                };
                query.lists.push(id);
            }
            "status" => {
                if status_seen {
                    return Err(Invalid::new("`status` is given more than once"));
                }
                status_seen = true;
                query.status = match value.as_ref() {
                    "open" => ShowFilter::Open,
                    "completed" => ShowFilter::Completed,
                    "all" => ShowFilter::All,
                    _other => {
                        return Err(Invalid::new("`status` must be open, completed or all"));
                    }
                };
            }
            unknown => {
                return Err(Invalid::new(format!("unknown query parameter `{unknown}`")));
            }
        }
    }
    Ok(query)
}

/// Parses the query of `GET /v1/mail/messages`; `accounts` holds the requested names, empty for all.
pub fn mail_messages_query(raw: Option<&str>) -> Result<MessageQuery, Invalid> {
    let mut accounts = Vec::new();
    let mut params = Params {
        single: Vec::new(),
        calendars: Vec::new(),
    };
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or("").as_bytes()) {
        if key == "account" {
            let Ok(name) = AccountName::parse(&value) else {
                return Err(Invalid::new(format!("unknown mail account {value:?}")));
            };
            accounts.push(name);
            continue;
        }
        params.insert(
            (key.into_owned(), value.into_owned()),
            &[
                "mailbox", "since", "until", "q", "unread", "limit", "cursor",
            ],
        )?;
    }
    let mailbox = match params.get("mailbox") {
        Some("") => return Err(Invalid::new("`mailbox` must not be empty")),
        Some(mailbox) => {
            check_text("mailbox", mailbox, MAX_MAILBOX)?;
            Some(mailbox.to_owned())
        }
        None => None,
    };
    let since = params.timestamp("since")?;
    let until = params.timestamp("until")?;
    if let (Some(since), Some(until)) = (since, until) {
        check_range(SINCE_UNTIL, since, until)?;
    }
    let q = match params.get("q") {
        Some(q) => {
            check_text("q", q, MAX_QUERY)?;
            Some(q.to_owned())
        }
        None => None,
    };
    let read = match params.get("unread") {
        None | Some("false") => ReadFilter::Any,
        Some("true") => ReadFilter::Unread,
        Some(_) => return Err(Invalid::new("`unread` must be true or false")),
    };
    let limit = params.number("limit", MAIL_LIMIT_BOUNDS)?;
    let cursor = match params.get("cursor") {
        Some(cursor) => match Cursor::decode(cursor) {
            Ok(cursor) => Some(cursor),
            Err(CursorError) => {
                return Err(Invalid::new(
                    "`cursor` must be the `next_cursor` of a previous page",
                ));
            }
        },
        None => None,
    };
    Ok(MessageQuery {
        accounts,
        mailbox,
        since,
        until,
        q,
        read,
        limit,
        cursor,
    })
}

/// Parses the id of `GET /v1/mail/messages/{id}`.
pub fn mail_message_id(value: &str) -> Result<MessageId, Invalid> {
    let Some(id) = MessageId::parse(value) else {
        return Err(Invalid::new("message id must be a positive integer"));
    };
    Ok(id)
}

/// The body of `POST /v1/reminders`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateReminderBody {
    /// The list to create the reminder in; must be a write list.
    pub list: String,
    /// The title.
    pub title: String,
    /// The notes.
    pub notes: Option<String>,
    /// The due date: RFC 3339 date-time or `YYYY-MM-DD`.
    pub due: Option<String>,
    /// The repeat rule; needs `due`.
    pub repeat: Option<String>,
    /// The priority.
    pub priority: Option<String>,
    /// A configured place for a location trigger.
    pub place: Option<String>,
    /// When the location trigger fires; needs `place`.
    pub proximity: Option<String>,
}

/// The body of `PATCH /v1/reminders/{id}`; `due` and `repeat` set to `null` are removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateReminderBody {
    /// The new title.
    pub title: Option<String>,
    /// The new notes.
    pub notes: Option<String>,
    /// The new due date, or `null` to remove it.
    #[serde(default, deserialize_with = "present")]
    pub due: Option<Option<String>>,
    /// The new repeat rule, or `null` to remove it.
    #[serde(default, deserialize_with = "present")]
    pub repeat: Option<Option<String>>,
    /// The new priority.
    pub priority: Option<String>,
    /// The new completion state.
    pub completed: Option<bool>,
}

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn parse_due(value: &str) -> Result<Due, Invalid> {
    if value.len() == 10 {
        let Ok(day) = NaiveDate::parse_from_str(value, "%Y-%m-%d") else {
            return Err(Invalid::new(
                "`due` must be an RFC 3339 timestamp or YYYY-MM-DD",
            ));
        };
        return Ok(Due::Day(day));
    }
    let Ok(at) = DateTime::parse_from_rfc3339(value) else {
        return Err(Invalid::new(
            "`due` must be an RFC 3339 timestamp or YYYY-MM-DD",
        ));
    };
    if at.nanosecond() != 0 {
        return Err(Invalid::new("`due` must not have fractional seconds"));
    }
    Ok(Due::At(at))
}

fn parse_repeat(value: &str) -> Result<Repeat, Invalid> {
    match value {
        "daily" => Ok(Repeat::Daily),
        "weekly" => Ok(Repeat::Weekly),
        "biweekly" => Ok(Repeat::Biweekly),
        "monthly" => Ok(Repeat::Monthly),
        "yearly" => Ok(Repeat::Yearly),
        _other => Err(Invalid::new(
            "`repeat` must be daily, weekly, biweekly, monthly or yearly",
        )),
    }
}

fn parse_priority(value: &str) -> Result<Priority, Invalid> {
    match value {
        "none" => Ok(Priority::None),
        "low" => Ok(Priority::Low),
        "medium" => Ok(Priority::Medium),
        "high" => Ok(Priority::High),
        _other => Err(Invalid::new("`priority` must be none, low, medium or high")),
    }
}

fn parse_proximity(value: &str) -> Result<Proximity, Invalid> {
    match value {
        "arriving" => Ok(Proximity::Arriving),
        "leaving" => Ok(Proximity::Leaving),
        _other => Err(Invalid::new("`proximity` must be arriving or leaving")),
    }
}

fn parse_place(value: &str, places: &[Place]) -> Result<Place, Invalid> {
    for place in places {
        if place.name.as_str() == value {
            return Ok(place.clone());
        }
    }
    Err(Invalid::new("`place` must be a configured place name"))
}

fn parse_trigger(
    place: Option<String>,
    proximity: Option<String>,
    places: &[Place],
) -> Result<Option<Trigger>, Invalid> {
    let Some(place) = place else {
        if proximity.is_some() {
            return Err(Invalid::new("`proximity` needs `place`"));
        }
        return Ok(None);
    };
    let place = parse_place(&place, places)?;
    let proximity = match proximity {
        Some(proximity) => parse_proximity(&proximity)?,
        None => Proximity::Arriving,
    };
    Ok(Some(Trigger { place, proximity }))
}

fn repeat_needs_due() -> Invalid {
    Invalid::new("`repeat` needs `due`")
}

/// Parses and validates a `POST /v1/reminders` body, resolving `place` against `places`.
pub fn create_reminder_body(body: &[u8], places: &[Place]) -> Result<NewReminder, Invalid> {
    let body: CreateReminderBody = parse_json(body)?;
    let CreateReminderBody {
        list,
        title,
        notes,
        due,
        repeat,
        priority,
        place,
        proximity,
    } = body;
    let Ok(list) = ListId::parse(list) else {
        return Err(Invalid::new("`list` must be a list id"));
    };
    let title = parse_title(title)?;
    let notes = parse_optional_text("notes", notes, MAX_NOTES)?;
    let due = match due {
        Some(due) => Some(parse_due(&due)?),
        None => None,
    };
    let repeat = match repeat {
        Some(repeat) => Some(parse_repeat(&repeat)?),
        None => None,
    };
    if repeat.is_some() && due.is_none() {
        return Err(repeat_needs_due());
    }
    let priority = match priority {
        Some(priority) => Some(parse_priority(&priority)?),
        None => None,
    };
    let location = parse_trigger(place, proximity, places)?;
    Ok(NewReminder {
        list,
        title,
        notes,
        due,
        repeat,
        priority,
        location,
    })
}

/// Parses and validates a `PATCH /v1/reminders/{id}` body, which must change at least one field.
pub fn update_reminder_body(body: &[u8]) -> Result<ReminderChanges, Invalid> {
    let body: UpdateReminderBody = parse_json(body)?;
    let UpdateReminderBody {
        title,
        notes,
        due,
        repeat,
        priority,
        completed,
    } = body;
    let title = match title {
        Some(title) => Some(parse_title(title)?),
        None => None,
    };
    let due = match due {
        None => None,
        Some(None) => Some(Change::Clear),
        Some(Some(due)) => Some(Change::Set(parse_due(&due)?)),
    };
    let repeat = match repeat {
        None => None,
        Some(None) => Some(Change::Clear),
        Some(Some(repeat)) => Some(Change::Set(parse_repeat(&repeat)?)),
    };
    let priority = match priority {
        Some(priority) => Some(parse_priority(&priority)?),
        None => None,
    };
    let completion = match completed {
        None => None,
        Some(true) => Some(Completion::Complete),
        Some(false) => Some(Completion::Incomplete),
    };
    let changes = ReminderChanges {
        title,
        notes: parse_optional_text("notes", notes, MAX_NOTES)?,
        due,
        repeat,
        priority,
        completion,
    };
    if changes == ReminderChanges::default() {
        return Err(Invalid::new("the update changes no field"));
    }
    Ok(changes)
}

/// Refuses `changes` that would leave `existing` with a repeat rule but no due date.
pub fn merged_repeat(changes: &ReminderChanges, existing: &RcReminder) -> Result<(), Invalid> {
    let has_due = match changes.due {
        Some(Change::Set(_)) => true,
        Some(Change::Clear) => false,
        None => existing.due_date.is_some(),
    };
    let has_repeat = match changes.repeat {
        Some(Change::Set(_)) => true,
        Some(Change::Clear) => false,
        None => existing.recurrence_rule.is_some(),
    };
    let touched = changes.due.is_some() || changes.repeat.is_some();
    if touched && has_repeat && !has_due {
        return Err(repeat_needs_due());
    }
    Ok(())
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
            "calendar": "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10",
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
        assert_eq!(
            free_query(Some("weekdays=Weekends")).unwrap().weekdays,
            Weekdays::Weekends
        );
        assert_eq!(
            free_query(Some("weekdays=ALL")).unwrap().weekdays,
            Weekdays::All
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
        for one_sided in ["from=2026-10-05T00:00:00Z", "to=2099-01-01T00:00:00Z"] {
            assert_eq!(
                message(free_query(Some(one_sided))),
                "`from` and `to` must be given together",
                "{one_sided}"
            );
        }
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
        let request = create_body(&body(valid_create())).unwrap();
        assert_eq!(
            request,
            CreateRequest {
                calendar: CalendarId::parse("8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10".to_owned())
                    .unwrap(),
                event: NewEvent {
                    title: "Lunch".to_owned(),
                    start: at("2026-10-05T12:00:00+02:00"),
                    end: at("2026-10-05T13:00:00+02:00"),
                    location: None,
                    notes: None,
                    url: None,
                },
            }
        );
        let CreateRequest { calendar: _, event } = create_body(&body(json!({
            "calendar": "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10",
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
    fn create_calendar_rules() {
        assert_eq!(
            message(create_body(&with(valid_create(), "calendar", json!(" ")))),
            "`calendar` must be a calendar id"
        );
        let mut missing = valid_create();
        missing.as_object_mut().unwrap().remove("calendar");
        assert!(create_body(&body(missing)).is_err());
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
                json!({"calendar": "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10", "title": "x", "start": "2026-10-05T12:00:00Z"})
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

    const LIST_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";

    fn places() -> Vec<Place> {
        crate::config::Config::from_toml(
            "listen = \"127.0.0.1:8790\"\n[places]\nshop = { address = \"1 Example Street\", radius = 150 }\n",
        )
        .unwrap()
        .places
    }

    fn existing(due: bool, repeat: bool) -> RcReminder {
        let mut value: serde_json::Value =
            serde_json::from_str(&crate::fake_ekctl::fixture_text("remindctl_info.json")).unwrap();
        if !due {
            value.as_object_mut().unwrap().remove("dueDate");
        }
        if !repeat {
            value.as_object_mut().unwrap().remove("recurrenceRule");
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn reminders_query_defaults_and_values() {
        assert_eq!(
            reminders_query(None),
            Ok(RemindersQuery {
                status: ShowFilter::Open,
                lists: Vec::new(),
            })
        );
        let query =
            reminders_query(Some(&format!("list={LIST_ID}&status=all&list={READ_ID}"))).unwrap();
        assert_eq!(query.status, ShowFilter::All);
        assert_eq!(
            query.lists,
            vec![
                ListId::parse(LIST_ID.to_owned()).unwrap(),
                ListId::parse(READ_ID.to_owned()).unwrap()
            ]
        );
        assert_eq!(
            reminders_query(Some("status=completed")).unwrap().status,
            ShowFilter::Completed
        );
    }

    #[test]
    fn reminders_query_errors() {
        assert_eq!(
            message(reminders_query(Some("status=closed"))),
            "`status` must be open, completed or all"
        );
        assert_eq!(
            message(reminders_query(Some("status=all&status=open"))),
            "`status` is given more than once"
        );
        assert_eq!(
            message(reminders_query(Some("list=2"))),
            "`list` must be a list id"
        );
        assert_eq!(
            message(reminders_query(Some("calendar=x"))),
            "unknown query parameter `calendar`"
        );
    }

    #[test]
    fn due_forms() {
        assert_eq!(
            parse_due("2026-10-06T09:00:00+02:00"),
            Ok(Due::At(at("2026-10-06T09:00:00+02:00")))
        );
        assert_eq!(
            parse_due("2026-10-07"),
            Ok(Due::Day(NaiveDate::from_ymd_opt(2026, 10, 7).unwrap()))
        );
        for bad in ["2026-10-6", "2026-02-30", "2026-10-06 09:00", "soon", ""] {
            assert_eq!(
                message(parse_due(bad)),
                "`due` must be an RFC 3339 timestamp or YYYY-MM-DD",
                "{bad:?}"
            );
        }
        assert_eq!(
            message(parse_due("2026-10-06T09:00:00.001Z")),
            "`due` must not have fractional seconds"
        );
    }

    #[test]
    fn create_reminder_resolves_the_place() {
        let reminder = create_reminder_body(
            &body(json!({"list": LIST_ID, "title": "Milk", "place": "shop"})),
            &places(),
        )
        .unwrap();
        let Some(Trigger { place, proximity }) = reminder.location else {
            panic!("no trigger");
        };
        assert_eq!(place.name.as_str(), "shop");
        assert_eq!(place.radius, 150);
        assert_eq!(proximity, Proximity::Arriving);
        assert_eq!(
            message(create_reminder_body(
                &body(json!({"list": LIST_ID, "title": "Milk", "place": "shop"})),
                &[],
            )),
            "`place` must be a configured place name"
        );
    }

    #[test]
    fn update_reminder_tells_null_from_absent() {
        let changes = update_reminder_body(&body(json!({"due": null}))).unwrap();
        assert_eq!(changes.due, Some(Change::Clear));
        assert_eq!(changes.repeat, None);
        let changes = update_reminder_body(&body(json!({"repeat": null}))).unwrap();
        assert_eq!(changes.repeat, Some(Change::Clear));
        assert_eq!(changes.due, None);
        let changes = update_reminder_body(&body(json!({"repeat": "monthly"}))).unwrap();
        assert_eq!(changes.repeat, Some(Change::Set(Repeat::Monthly)));
        assert_eq!(
            message(update_reminder_body(&body(json!({"priority": null})))),
            "the update changes no field"
        );
    }

    #[test]
    fn merged_repeat_needs_a_due_date() {
        let clear_due = ReminderChanges {
            due: Some(Change::Clear),
            ..ReminderChanges::default()
        };
        let set_repeat = ReminderChanges {
            repeat: Some(Change::Set(Repeat::Daily)),
            ..ReminderChanges::default()
        };
        let clear_both = ReminderChanges {
            due: Some(Change::Clear),
            repeat: Some(Change::Clear),
            ..ReminderChanges::default()
        };
        let rename = ReminderChanges {
            title: Some("x".to_owned()),
            ..ReminderChanges::default()
        };
        assert_eq!(
            message(merged_repeat(&clear_due, &existing(true, true))),
            "`repeat` needs `due`"
        );
        assert_eq!(merged_repeat(&clear_due, &existing(true, false)), Ok(()));
        assert_eq!(merged_repeat(&set_repeat, &existing(true, false)), Ok(()));
        assert_eq!(
            message(merged_repeat(&set_repeat, &existing(false, false))),
            "`repeat` needs `due`"
        );
        assert_eq!(merged_repeat(&clear_both, &existing(true, true)), Ok(()));
        assert_eq!(merged_repeat(&rename, &existing(false, true)), Ok(()));
    }
    fn account(value: &str) -> AccountName {
        AccountName::parse(value).unwrap()
    }

    #[test]
    fn mail_messages_query_defaults() {
        for raw in [None, Some("")] {
            assert_eq!(mail_messages_query(raw), Ok(MessageQuery::default()));
        }
    }

    #[test]
    fn mail_messages_query_full() {
        let cursor = Cursor::decode("MTc5MTIwMDEwMDo4MzA").unwrap();
        let query = mail_messages_query(Some(
            "account=main&account=gmail&mailbox=%5BGmail%5D%2FAll%20Mail&since=2026-10-01T00:00:00+02:00&until=2026-10-05T00:00:00Z&q=%D0%9F%D1%80%D0%B8%D0%B2%D0%B5%D1%82&unread=true&limit=100&cursor=MTc5MTIwMDEwMDo4MzA",
        ))
        .unwrap();
        assert_eq!(
            query,
            MessageQuery {
                accounts: vec![account("main"), account("gmail")],
                mailbox: Some("[Gmail]/All Mail".to_owned()),
                since: Some(at("2026-10-01T00:00:00+02:00")),
                until: Some(at("2026-10-05T00:00:00Z")),
                q: Some("Привет".to_owned()),
                read: ReadFilter::Unread,
                limit: 100,
                cursor: Some(cursor),
            }
        );
        let query =
            mail_messages_query(Some("unread=false&limit=1&since=2026-10-01T00:00:00Z")).unwrap();
        assert_eq!(query.read, ReadFilter::Any);
        assert_eq!(query.limit, 1);
        assert_eq!(query.until, None);
    }

    #[test]
    fn mail_messages_query_rejects_bad_values() {
        let cases = [
            ("account=Main", "unknown mail account \"Main\""),
            ("account=", "unknown mail account \"\""),
            ("mailbox=", "`mailbox` must not be empty"),
            (
                "mailbox=a%00b",
                "`mailbox` must not contain control characters",
            ),
            ("since=yesterday", "`since` must be an RFC 3339 timestamp"),
            ("until=2026-10-05", "`until` must be an RFC 3339 timestamp"),
            (
                "since=2026-10-05T00:00:00Z&until=2026-10-05T00:00:00Z",
                "`until` must be after `since`",
            ),
            (
                "since=2026-10-05T00:00:00Z&until=2026-10-04T00:00:00Z",
                "`until` must be after `since`",
            ),
            ("unread=yes", "`unread` must be true or false"),
            ("unread=", "`unread` must be true or false"),
            ("limit=0", "`limit` must be an integer from 1 to 100"),
            ("limit=101", "`limit` must be an integer from 1 to 100"),
            ("limit=ten", "`limit` must be an integer from 1 to 100"),
            (
                "cursor=not-a-cursor",
                "`cursor` must be the `next_cursor` of a previous page",
            ),
            (
                "cursor=",
                "`cursor` must be the `next_cursor` of a previous page",
            ),
            (
                "from=2026-10-05T00:00:00Z",
                "unknown query parameter `from`",
            ),
            ("q=a&q=b", "`q` is given more than once"),
            (
                "mailbox=Inbox&mailbox=Sent",
                "`mailbox` is given more than once",
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(message(mail_messages_query(Some(raw))), expected, "{raw}");
        }
        let long = "x".repeat(MAX_QUERY + 1);
        assert_eq!(
            message(mail_messages_query(Some(&format!("q={long}")))),
            "`q` must be at most 500 characters"
        );
    }

    #[test]
    fn mail_message_id_accepts_positive_integers_only() {
        assert_eq!(
            mail_message_id("383621"),
            Ok(MessageId::new(383621).unwrap())
        );
        for value in ["0", "-1", "abc", "1.5", "", "01"] {
            assert_eq!(
                message(mail_message_id(value)),
                "message id must be a positive integer",
                "{value}"
            );
        }
    }
}
