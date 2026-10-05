use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveTime, SecondsFormat, Weekday};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, MutexGuard};
use tokio::time;
use url::Url;

use crate::config::CalendarId;
use crate::model::{
    EkCalendar, EkCalendarList, EkDeleted, EkEventEnvelope, EkEventList, EkFree, EkWritten,
    EkWrittenEvent, Event, EventId, FreeSlots,
};

/// How long one `ekctl` invocation may run before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

const STDOUT_CAP: usize = 8 * 1024 * 1024;
const STDERR_TAIL: usize = 500;
const READ_CHUNK: usize = 64 * 1024;
const EVENT_NOT_FOUND: &str = "Event not found";

/// Why an `ekctl` invocation produced no usable result.
#[derive(Debug, thiserror::Error)]
pub enum EkctlError {
    /// `ekctl` could not be started.
    #[error("ekctl could not start: {0}")]
    Spawn(#[source] io::Error),
    /// Reading `ekctl`'s output or waiting for it failed.
    #[error("ekctl i/o failed: {0}")]
    Io(#[source] io::Error),
    /// `ekctl` ran past its deadline and was killed.
    #[error("ekctl timed out")]
    Timeout,
    /// `ekctl` wrote more than the stdout cap and was killed.
    #[error("output too large")]
    OutputTooLarge,
    /// `ekctl` exited unsuccessfully.
    #[error("{}", exit_message(*.code, .reason))]
    Exit {
        /// The exit code, `None` when a signal ended the process.
        code: Option<i32>,
        /// The error envelope's message when stdout carries one, otherwise the last bytes of stderr.
        reason: String,
    },
    /// `show event` reported that the event does not exist.
    #[error("{0}")]
    NotFound(String),
    /// `ekctl` exited `0` but reported an error.
    #[error("ekctl: {0}")]
    Reported(String),
    /// `ekctl`'s stdout was not the JSON shape the command produces.
    #[error("unexpected ekctl output")]
    UnexpectedOutput,
}

fn exit_message(code: Option<i32>, reason: &str) -> String {
    let status = match code {
        Some(code) => format!("ekctl exited with code {code}"),
        None => "ekctl was killed by a signal".to_owned(),
    };
    if reason.is_empty() {
        return status;
    }
    format!("{status}: {reason}")
}

/// The calendars and time range of an events query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRange {
    /// The calendars to read.
    pub calendars: Vec<CalendarId>,
    /// The range start.
    pub from: DateTime<FixedOffset>,
    /// The range end.
    pub to: DateTime<FixedOffset>,
}

/// The daily window a free-slot search considers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingHours {
    /// The whole day.
    All,
    /// From `start` to `end` local time.
    Window {
        /// The window start.
        start: NaiveTime,
        /// The window end.
        end: NaiveTime,
    },
}

/// The days a free-slot search considers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Weekdays {
    /// Monday to Friday.
    Weekdays,
    /// Saturday and Sunday.
    Weekends,
    /// Every day.
    All,
    /// The listed days.
    Days(Vec<Weekday>),
}

/// A free-slot search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeQuery {
    /// The calendars whose events count as busy.
    pub calendars: Vec<CalendarId>,
    /// The minimum slot length in minutes.
    pub duration_minutes: u32,
    /// The daily window.
    pub working_hours: WorkingHours,
    /// The days searched.
    pub weekdays: Weekdays,
    /// The gap kept around busy events, in minutes.
    pub buffer_minutes: u32,
    /// The most slots returned.
    pub limit: u32,
    /// The range start; `ekctl` defaults to now.
    pub from: Option<DateTime<FixedOffset>>,
    /// The range end; `ekctl` defaults to seven days after the start.
    pub to: Option<DateTime<FixedOffset>>,
}

/// A timed event to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    /// The title.
    pub title: String,
    /// The start.
    pub start: DateTime<FixedOffset>,
    /// The end.
    pub end: DateTime<FixedOffset>,
    /// The location.
    pub location: Option<String>,
    /// The notes.
    pub notes: Option<String>,
    /// The url.
    pub url: Option<Url>,
}

/// The fields of an event to change; `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventChanges {
    /// The new title.
    pub title: Option<String>,
    /// The new start.
    pub start: Option<DateTime<FixedOffset>>,
    /// The new end.
    pub end: Option<DateTime<FixedOffset>>,
    /// The new location.
    pub location: Option<String>,
    /// The new notes.
    pub notes: Option<String>,
    /// The new url.
    pub url: Option<Url>,
}

/// The `ekctl` subcommands the bridge runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subcommand {
    /// `list calendars`.
    ListCalendars,
    /// `list events`.
    ListEvents,
    /// `show event`.
    ShowEvent,
    /// `free`.
    Free,
    /// `add event`.
    AddEvent,
    /// `update event`.
    UpdateEvent,
    /// `delete event`.
    DeleteEvent,
}

impl Subcommand {
    /// The subcommand as `ekctl` spells it, for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Subcommand::ListCalendars => "list calendars",
            Subcommand::ListEvents => "list events",
            Subcommand::ShowEvent => "show event",
            Subcommand::Free => "free",
            Subcommand::AddEvent => "add event",
            Subcommand::UpdateEvent => "update event",
            Subcommand::DeleteEvent => "delete event",
        }
    }

    fn words(self) -> &'static [&'static str] {
        match self {
            Subcommand::ListCalendars => &["list", "calendars"],
            Subcommand::ListEvents => &["list", "events"],
            Subcommand::ShowEvent => &["show", "event"],
            Subcommand::Free => &["free"],
            Subcommand::AddEvent => &["add", "event"],
            Subcommand::UpdateEvent => &["update", "event"],
            Subcommand::DeleteEvent => &["delete", "event"],
        }
    }
}

/// How one `ekctl` invocation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    /// `ekctl` could not be started.
    NotStarted,
    /// `ekctl` exited with this code.
    Exited(i32),
    /// A signal ended `ekctl`.
    Signalled,
    /// `ekctl` ran past its deadline and was killed.
    TimedOut,
    /// The bridge stopped reading and killed `ekctl`.
    Killed,
}

/// One `ekctl` invocation, as the request log reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    /// The subcommand that ran.
    pub subcommand: Subcommand,
    /// How it ended.
    pub outcome: CallOutcome,
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Call {
            subcommand,
            outcome,
        } = self;
        let subcommand = subcommand.as_str();
        match outcome {
            CallOutcome::NotStarted => write!(f, "{subcommand}=not started"),
            CallOutcome::Exited(code) => write!(f, "{subcommand}={code}"),
            CallOutcome::Signalled => write!(f, "{subcommand}=signalled"),
            CallOutcome::TimedOut => write!(f, "{subcommand}=timeout"),
            CallOutcome::Killed => write!(f, "{subcommand}=killed"),
        }
    }
}

tokio::task_local! {
    static CALLS: CallLog;
}

/// Collects the `ekctl` invocations made by a future run through [`CallLog::scope`].
#[derive(Debug, Clone, Default)]
pub struct CallLog(Arc<std::sync::Mutex<Vec<Call>>>);

impl CallLog {
    /// Runs `future`, recording every `ekctl` invocation it makes on this task.
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        CALLS.scope(self.clone(), future).await
    }

    /// The invocations recorded so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(subcommand: Subcommand, outcome: CallOutcome) {
        let recorded = CALLS.try_with(|log| {
            log.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Call {
                    subcommand,
                    outcome,
                });
        });
        recorded.ok();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    subcommand: Subcommand,
    args: Vec<String>,
}

impl Invocation {
    fn new(subcommand: Subcommand) -> Self {
        let mut args = Vec::new();
        for word in subcommand.words() {
            args.push((*word).to_owned());
        }
        Self { subcommand, args }
    }

    fn option(&mut self, name: &str, value: &str) {
        self.args.push(format!("--{name}={value}"));
    }

    fn optional(&mut self, name: &str, value: Option<&str>) {
        if let Some(value) = value {
            self.option(name, value);
        }
    }

    fn event_id(&mut self, id: &EventId) {
        self.args.push("--".to_owned());
        self.args.push(id.as_str().to_owned());
    }
}

fn calendar_list(calendars: &[CalendarId]) -> String {
    let mut list = String::new();
    for id in calendars {
        if !list.is_empty() {
            list.push(',');
        }
        list.push_str(id.as_str());
    }
    list
}

fn timestamp(value: DateTime<FixedOffset>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, false)
}

fn optional_timestamp(value: Option<DateTime<FixedOffset>>) -> Option<String> {
    let value = value?;
    Some(timestamp(value))
}

fn working_hours(hours: WorkingHours) -> String {
    match hours {
        WorkingHours::All => "all".to_owned(),
        WorkingHours::Window { start, end } => {
            format!("{}-{}", start.format("%H:%M"), end.format("%H:%M"))
        }
    }
}

fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "monday",
        Weekday::Tue => "tuesday",
        Weekday::Wed => "wednesday",
        Weekday::Thu => "thursday",
        Weekday::Fri => "friday",
        Weekday::Sat => "saturday",
        Weekday::Sun => "sunday",
    }
}

fn weekdays(days: &Weekdays) -> String {
    match days {
        Weekdays::Weekdays => "weekdays".to_owned(),
        Weekdays::Weekends => "weekends".to_owned(),
        Weekdays::All => "all".to_owned(),
        Weekdays::Days(days) => {
            let mut list = String::new();
            for day in days {
                if !list.is_empty() {
                    list.push(',');
                }
                list.push_str(weekday_name(*day));
            }
            list
        }
    }
}

fn list_calendars() -> Invocation {
    Invocation::new(Subcommand::ListCalendars)
}

fn list_events(range: &EventRange) -> Invocation {
    let EventRange {
        calendars,
        from,
        to,
    } = range;
    let mut invocation = Invocation::new(Subcommand::ListEvents);
    invocation.option("calendar", &calendar_list(calendars));
    invocation.option("from", &timestamp(*from));
    invocation.option("to", &timestamp(*to));
    invocation
}

fn show_event(id: &EventId) -> Invocation {
    let mut invocation = Invocation::new(Subcommand::ShowEvent);
    invocation.event_id(id);
    invocation
}

fn free(query: &FreeQuery) -> Invocation {
    let FreeQuery {
        calendars,
        duration_minutes,
        working_hours: hours,
        weekdays: days,
        buffer_minutes,
        limit,
        from,
        to,
    } = query;
    let mut invocation = Invocation::new(Subcommand::Free);
    invocation.option("calendar", &calendar_list(calendars));
    invocation.option("duration", &duration_minutes.to_string());
    invocation.option("working-hours", &working_hours(*hours));
    invocation.option("weekdays", &weekdays(days));
    invocation.option("buffer", &buffer_minutes.to_string());
    invocation.option("limit", &limit.to_string());
    invocation.optional("from", optional_timestamp(*from).as_deref());
    invocation.optional("to", optional_timestamp(*to).as_deref());
    invocation
}

fn add_event(calendar: &CalendarId, event: &NewEvent) -> Invocation {
    let NewEvent {
        title,
        start,
        end,
        location,
        notes,
        url,
    } = event;
    let mut invocation = Invocation::new(Subcommand::AddEvent);
    invocation.option("calendar", calendar.as_str());
    invocation.option("title", title);
    invocation.option("start", &timestamp(*start));
    invocation.option("end", &timestamp(*end));
    invocation.optional("location", location.as_deref());
    invocation.optional("notes", notes.as_deref());
    invocation.optional("url", url.as_ref().map(Url::as_str));
    invocation
}

fn update_event(id: &EventId, changes: &EventChanges) -> Invocation {
    let EventChanges {
        title,
        start,
        end,
        location,
        notes,
        url,
    } = changes;
    let mut invocation = Invocation::new(Subcommand::UpdateEvent);
    invocation.optional("title", title.as_deref());
    invocation.optional("start", optional_timestamp(*start).as_deref());
    invocation.optional("end", optional_timestamp(*end).as_deref());
    invocation.optional("location", location.as_deref());
    invocation.optional("notes", notes.as_deref());
    invocation.optional("url", url.as_ref().map(Url::as_str));
    invocation.event_id(id);
    invocation
}

fn delete_event(id: &EventId) -> Invocation {
    let mut invocation = Invocation::new(Subcommand::DeleteEvent);
    invocation.event_id(id);
    invocation
}

/// Runs `ekctl`, one invocation at a time.
#[derive(Debug)]
pub struct Runner {
    program: PathBuf,
    timeout: Duration,
    lock: Mutex<()>,
}

/// Exclusive use of the runner; every call made through one session runs under the same lock.
#[derive(Debug)]
pub struct Session<'a> {
    runner: &'a Runner,
    _guard: MutexGuard<'a, ()>,
}

struct Collected {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: ExitStatus,
}

impl Runner {
    /// A runner for the `ekctl` at `program`, killing any invocation that outlives `timeout`.
    pub fn new(program: PathBuf, timeout: Duration) -> Self {
        Self {
            program,
            timeout,
            lock: Mutex::new(()),
        }
    }

    /// Waits for every other session to end and starts a new one.
    pub async fn session(&self) -> Session<'_> {
        let guard = self.lock.lock().await;
        Session {
            runner: self,
            _guard: guard,
        }
    }

    async fn run(&self, invocation: &Invocation) -> Result<Vec<u8>, EkctlError> {
        let result = self.run_child(invocation).await;
        CallLog::record(invocation.subcommand, outcome(&result));
        result
    }

    async fn run_child(&self, invocation: &Invocation) -> Result<Vec<u8>, EkctlError> {
        let mut child = Command::new(&self.program)
            .args(&invocation.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(EkctlError::Spawn)?;
        let collected = match time::timeout(self.timeout, collect(&mut child)).await {
            Ok(collected) => collected,
            Err(_elapsed) => Err(EkctlError::Timeout),
        };
        let Collected {
            stdout,
            stderr,
            status,
        } = match collected {
            Ok(collected) => collected,
            Err(err) => {
                kill(&mut child).await;
                return Err(err);
            }
        };
        if !status.success() {
            let reason = match envelope_error(&stdout) {
                Some(message) => message,
                None => String::from_utf8_lossy(&stderr).trim().to_owned(),
            };
            return Err(EkctlError::Exit {
                code: status.code(),
                reason,
            });
        }
        Ok(stdout)
    }
}

impl Session<'_> {
    /// Every calendar and reminder list `ekctl` can see.
    pub async fn list_calendars(&self) -> Result<Vec<EkCalendar>, EkctlError> {
        let EkCalendarList { calendars } = self.execute(&list_calendars()).await?;
        Ok(calendars)
    }

    /// The events in `range`, in `ekctl`'s order.
    pub async fn list_events(&self, range: &EventRange) -> Result<Vec<Event>, EkctlError> {
        let EkEventList { events } = self.execute(&list_events(range)).await?;
        let mut converted = Vec::new();
        for event in events {
            converted.push(Event::from(event));
        }
        Ok(converted)
    }

    /// One event; [`EkctlError::NotFound`] when it does not exist.
    pub async fn show_event(&self, id: &EventId) -> Result<Event, EkctlError> {
        let EkEventEnvelope { event } = self.execute(&show_event(id)).await?;
        Ok(Event::from(event))
    }

    /// The free slots matching `query`.
    pub async fn free(&self, query: &FreeQuery) -> Result<FreeSlots, EkctlError> {
        let free: EkFree = self.execute(&free(query)).await?;
        Ok(FreeSlots::from(free))
    }

    /// Creates `event` in `calendar` and returns the new event's id.
    pub async fn add_event(
        &self,
        calendar: &CalendarId,
        event: &NewEvent,
    ) -> Result<EventId, EkctlError> {
        let EkWritten {
            event: EkWrittenEvent { id },
        } = self.execute(&add_event(calendar, event)).await?;
        Ok(id)
    }

    /// Applies `changes` to the event `id` and returns its id.
    pub async fn update_event(
        &self,
        id: &EventId,
        changes: &EventChanges,
    ) -> Result<EventId, EkctlError> {
        let EkWritten {
            event: EkWrittenEvent { id },
        } = self.execute(&update_event(id, changes)).await?;
        Ok(id)
    }

    /// Deletes the event `id`.
    pub async fn delete_event(&self, id: &EventId) -> Result<(), EkctlError> {
        let EkDeleted { status: _ } = self.execute(&delete_event(id)).await?;
        Ok(())
    }

    async fn execute<T: DeserializeOwned>(&self, invocation: &Invocation) -> Result<T, EkctlError> {
        let stdout = self.runner.run(invocation).await?;
        parse(invocation.subcommand, &stdout)
    }
}

fn outcome(result: &Result<Vec<u8>, EkctlError>) -> CallOutcome {
    let err = match result {
        Ok(_) => return CallOutcome::Exited(0),
        Err(err) => err,
    };
    match err {
        EkctlError::Spawn(_) => CallOutcome::NotStarted,
        EkctlError::Timeout => CallOutcome::TimedOut,
        EkctlError::Io(_) | EkctlError::OutputTooLarge => CallOutcome::Killed,
        EkctlError::Exit {
            code: Some(code),
            reason: _,
        } => CallOutcome::Exited(*code),
        EkctlError::Exit {
            code: None,
            reason: _,
        } => CallOutcome::Signalled,
        EkctlError::NotFound(_) | EkctlError::Reported(_) | EkctlError::UnexpectedOutput => {
            CallOutcome::Exited(0)
        }
    }
}

async fn collect(child: &mut Child) -> Result<Collected, EkctlError> {
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(EkctlError::Io(io::Error::other(
            "ekctl output pipes are missing",
        )));
    };
    let (stdout, stderr) = tokio::try_join!(read_capped(stdout), read_tail(stderr))?;
    let status = child.wait().await.map_err(EkctlError::Io)?;
    Ok(Collected {
        stdout,
        stderr,
        status,
    })
}

async fn kill(child: &mut Child) {
    if let Err(err) = child.kill().await {
        tracing::warn!(error = %err, "could not kill ekctl");
    }
}

async fn read_capped(mut pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>, EkctlError> {
    let mut output = Vec::new();
    let mut chunk = vec![0; READ_CHUNK];
    loop {
        let read = pipe.read(&mut chunk).await.map_err(EkctlError::Io)?;
        if read == 0 {
            return Ok(output);
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() > STDOUT_CAP {
            return Err(EkctlError::OutputTooLarge);
        }
    }
}

async fn read_tail(mut pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>, EkctlError> {
    let mut tail = Vec::new();
    let mut chunk = vec![0; READ_CHUNK];
    loop {
        let read = pipe.read(&mut chunk).await.map_err(EkctlError::Io)?;
        if read == 0 {
            return Ok(tail);
        }
        tail.extend_from_slice(&chunk[..read]);
        if tail.len() > STDERR_TAIL {
            let excess = tail.len() - STDERR_TAIL;
            tail.drain(..excess);
        }
    }
}

fn parse<T: DeserializeOwned>(subcommand: Subcommand, stdout: &[u8]) -> Result<T, EkctlError> {
    let Ok(value) = serde_json::from_slice::<Value>(stdout) else {
        return Err(EkctlError::UnexpectedOutput);
    };
    if value.get("status").and_then(Value::as_str) == Some("error") {
        let Some(message) = value.get("error").and_then(Value::as_str) else {
            return Err(EkctlError::UnexpectedOutput);
        };
        return Err(reported(subcommand, message));
    }
    serde_json::from_value(value).map_err(|_| EkctlError::UnexpectedOutput)
}

fn envelope_error(stdout: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(stdout).ok()?;
    if value.get("status").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let message = value.get("error").and_then(Value::as_str)?;
    Some(message.to_owned())
}

fn reported(subcommand: Subcommand, message: &str) -> EkctlError {
    let not_found = match subcommand {
        Subcommand::ShowEvent => message.starts_with(EVENT_NOT_FOUND),
        Subcommand::ListCalendars
        | Subcommand::ListEvents
        | Subcommand::Free
        | Subcommand::AddEvent
        | Subcommand::UpdateEvent
        | Subcommand::DeleteEvent => false,
    };
    if not_found {
        return EkctlError::NotFound(message.to_owned());
    }
    EkctlError::Reported(message.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::fake_ekctl::{Fake, fixture};
    use crate::model::CalendarKind;

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const EVENT_ID: &str = "46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076";

    fn calendar_id(value: &str) -> CalendarId {
        CalendarId::parse(value.to_owned()).unwrap()
    }

    fn event_id(value: &str) -> EventId {
        EventId::parse(value).unwrap()
    }

    fn at(value: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(value).unwrap()
    }

    fn strings(values: &[&str]) -> Vec<String> {
        let mut owned = Vec::new();
        for value in values {
            owned.push((*value).to_owned());
        }
        owned
    }

    fn range() -> EventRange {
        EventRange {
            calendars: vec![calendar_id(READ_ID), calendar_id(WRITE_ID)],
            from: at("2026-10-05T00:00:00+02:00"),
            to: at("2026-10-12T00:00:00+02:00"),
        }
    }

    fn new_event() -> NewEvent {
        NewEvent {
            title: "Lunch".to_owned(),
            start: at("2026-02-10T12:30:00Z"),
            end: at("2026-02-10T13:30:00Z"),
            location: None,
            notes: None,
            url: None,
        }
    }

    #[test]
    fn list_calendars_argv() {
        assert_eq!(list_calendars().args, strings(&["list", "calendars"]));
    }

    #[test]
    fn list_events_argv() {
        assert_eq!(
            list_events(&range()).args,
            strings(&[
                "list",
                "events",
                &format!("--calendar={READ_ID},{WRITE_ID}"),
                "--from=2026-10-05T00:00:00+02:00",
                "--to=2026-10-12T00:00:00+02:00",
            ])
        );
    }

    #[test]
    fn show_event_argv() {
        assert_eq!(
            show_event(&event_id(EVENT_ID)).args,
            strings(&["show", "event", "--", EVENT_ID])
        );
    }

    #[test]
    fn show_event_argv_with_dash_id() {
        assert_eq!(
            show_event(&event_id("--calendar=x")).args,
            strings(&["show", "event", "--", "--calendar=x"])
        );
    }

    #[test]
    fn free_argv_with_defaults_left_to_ekctl() {
        let query = FreeQuery {
            calendars: vec![calendar_id(READ_ID)],
            duration_minutes: 30,
            working_hours: WorkingHours::Window {
                start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                end: NaiveTime::from_hms_opt(17, 30, 0).unwrap(),
            },
            weekdays: Weekdays::Weekdays,
            buffer_minutes: 0,
            limit: 20,
            from: None,
            to: None,
        };
        assert_eq!(
            free(&query).args,
            strings(&[
                "free",
                &format!("--calendar={READ_ID}"),
                "--duration=30",
                "--working-hours=09:00-17:30",
                "--weekdays=weekdays",
                "--buffer=0",
                "--limit=20",
            ])
        );
    }

    #[test]
    fn free_argv_with_range_and_days() {
        let query = FreeQuery {
            calendars: vec![calendar_id(READ_ID), calendar_id(WRITE_ID)],
            duration_minutes: 60,
            working_hours: WorkingHours::All,
            weekdays: Weekdays::Days(vec![Weekday::Mon, Weekday::Wed, Weekday::Sun]),
            buffer_minutes: 15,
            limit: 5,
            from: Some(at("2026-10-05T08:00:00+02:00")),
            to: Some(at("2026-10-06T08:00:00Z")),
        };
        assert_eq!(
            free(&query).args,
            strings(&[
                "free",
                &format!("--calendar={READ_ID},{WRITE_ID}"),
                "--duration=60",
                "--working-hours=all",
                "--weekdays=monday,wednesday,sunday",
                "--buffer=15",
                "--limit=5",
                "--from=2026-10-05T08:00:00+02:00",
                "--to=2026-10-06T08:00:00+00:00",
            ])
        );
    }

    #[test]
    fn free_argv_weekday_keywords() {
        let mut query = FreeQuery {
            calendars: vec![calendar_id(READ_ID)],
            duration_minutes: 30,
            working_hours: WorkingHours::All,
            weekdays: Weekdays::Weekends,
            buffer_minutes: 0,
            limit: 1,
            from: None,
            to: Some(at("2026-10-06T08:00:00Z")),
        };
        assert!(
            free(&query)
                .args
                .contains(&"--weekdays=weekends".to_owned())
        );
        assert!(
            !free(&query)
                .args
                .iter()
                .any(|arg| arg.starts_with("--from"))
        );
        query.weekdays = Weekdays::All;
        assert!(free(&query).args.contains(&"--weekdays=all".to_owned()));
    }

    #[test]
    fn add_event_argv_minimal() {
        assert_eq!(
            add_event(&calendar_id(WRITE_ID), &new_event()).args,
            strings(&[
                "add",
                "event",
                &format!("--calendar={WRITE_ID}"),
                "--title=Lunch",
                "--start=2026-02-10T12:30:00+00:00",
                "--end=2026-02-10T13:30:00+00:00",
            ])
        );
    }

    #[test]
    fn add_event_argv_full_with_dash_title() {
        let event = NewEvent {
            title: "-rf --calendar=other".to_owned(),
            location: Some("-Room 1".to_owned()),
            notes: Some("line one\nline two".to_owned()),
            url: Some(Url::parse("https://example.com/a?b=c").unwrap()),
            ..new_event()
        };
        assert_eq!(
            add_event(&calendar_id(WRITE_ID), &event).args,
            strings(&[
                "add",
                "event",
                &format!("--calendar={WRITE_ID}"),
                "--title=-rf --calendar=other",
                "--start=2026-02-10T12:30:00+00:00",
                "--end=2026-02-10T13:30:00+00:00",
                "--location=-Room 1",
                "--notes=line one\nline two",
                "--url=https://example.com/a?b=c",
            ])
        );
    }

    #[test]
    fn update_event_argv_partial() {
        let changes = EventChanges {
            start: Some(at("2026-10-05T12:00:00+02:00")),
            ..EventChanges::default()
        };
        assert_eq!(
            update_event(&event_id(EVENT_ID), &changes).args,
            strings(&[
                "update",
                "event",
                "--start=2026-10-05T12:00:00+02:00",
                "--",
                EVENT_ID,
            ])
        );
    }

    #[test]
    fn update_event_argv_full() {
        let changes = EventChanges {
            title: Some("-Renamed".to_owned()),
            start: Some(at("2026-10-05T12:00:00+02:00")),
            end: Some(at("2026-10-05T13:00:00+02:00")),
            location: Some("Office".to_owned()),
            notes: Some("Bring\tlaptop".to_owned()),
            url: Some(Url::parse("http://example.com/").unwrap()),
        };
        assert_eq!(
            update_event(&event_id("-x"), &changes).args,
            strings(&[
                "update",
                "event",
                "--title=-Renamed",
                "--start=2026-10-05T12:00:00+02:00",
                "--end=2026-10-05T13:00:00+02:00",
                "--location=Office",
                "--notes=Bring\tlaptop",
                "--url=http://example.com/",
                "--",
                "-x",
            ])
        );
    }

    #[test]
    fn delete_event_argv() {
        assert_eq!(
            delete_event(&event_id(EVENT_ID)).args,
            strings(&["delete", "event", "--", EVENT_ID])
        );
    }

    #[tokio::test]
    async fn list_calendars_through_fake() {
        let fake = Fake::recording("list_calendars.json");
        let runner = fake.runner();
        let calendars = runner.session().await.list_calendars().await.unwrap();
        assert_eq!(calendars.len(), 3);
        assert_eq!(calendars[0].id, calendar_id(READ_ID));
        assert_eq!(calendars[2].kind, CalendarKind::Reminder);
        assert_eq!(fake.recorded_args(), strings(&["list", "calendars"]));
    }

    #[tokio::test]
    async fn list_events_through_fake() {
        let fake = Fake::recording("list_events.json");
        let runner = fake.runner();
        let events = runner.session().await.list_events(&range()).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event_id(EVENT_ID));
        assert_eq!(events[0].title.as_deref(), Some("Standup"));
        assert_eq!(events[0].recurring, Some(true));
        assert_eq!(fake.recorded_args(), list_events(&range()).args);
    }

    #[tokio::test]
    async fn show_event_through_fake() {
        let fake = Fake::printing("show_event.json");
        let runner = fake.runner();
        let event = runner
            .session()
            .await
            .show_event(&event_id(EVENT_ID))
            .await
            .unwrap();
        assert_eq!(event.calendar.id, calendar_id(READ_ID));
        assert_eq!(event.start.as_str(), "2026-10-05T11:00:00+02:00");
        assert_eq!(event.attendees.len(), 1);
    }

    #[tokio::test]
    async fn free_through_fake() {
        let fake = Fake::printing("free.json");
        let runner = fake.runner();
        let query = FreeQuery {
            calendars: vec![calendar_id(READ_ID)],
            duration_minutes: 60,
            working_hours: WorkingHours::All,
            weekdays: Weekdays::All,
            buffer_minutes: 0,
            limit: 20,
            from: None,
            to: None,
        };
        let free = runner.session().await.free(&query).await.unwrap();
        assert_eq!(free.slots.len(), 1);
        assert_eq!(free.slots[0].duration_minutes, Some(60));
        assert_eq!(free.searched_to.as_str(), "2026-10-07T21:40:21+02:00");
    }

    #[tokio::test]
    async fn add_event_passes_argv_verbatim() {
        let fake = Fake::recording("add_event.json");
        let runner = fake.runner();
        let event = NewEvent {
            title: "-rf".to_owned(),
            notes: Some("multi\nline 'quoted' $HOME".to_owned()),
            ..new_event()
        };
        let id = runner
            .session()
            .await
            .add_event(&calendar_id(WRITE_ID), &event)
            .await
            .unwrap();
        assert_eq!(id, event_id("NEW123:EVENT456"));
        assert_eq!(
            fake.recorded_args(),
            strings(&[
                "add",
                "event",
                &format!("--calendar={WRITE_ID}"),
                "--title=-rf",
                "--start=2026-02-10T12:30:00+00:00",
                "--end=2026-02-10T13:30:00+00:00",
                "--notes=multi\nline 'quoted' $HOME",
            ])
        );
    }

    #[tokio::test]
    async fn update_event_through_fake() {
        let fake = Fake::recording("add_event.json");
        let runner = fake.runner();
        let changes = EventChanges {
            title: Some("Lunch".to_owned()),
            ..EventChanges::default()
        };
        let id = runner
            .session()
            .await
            .update_event(&event_id("--"), &changes)
            .await
            .unwrap();
        assert_eq!(id, event_id("NEW123:EVENT456"));
        assert_eq!(
            fake.recorded_args(),
            strings(&["update", "event", "--title=Lunch", "--", "--"])
        );
    }

    #[tokio::test]
    async fn delete_event_through_fake() {
        let fake = Fake::recording("delete_event.json");
        let runner = fake.runner();
        runner
            .session()
            .await
            .delete_event(&event_id(EVENT_ID))
            .await
            .unwrap();
        assert_eq!(
            fake.recorded_args(),
            strings(&["delete", "event", "--", EVENT_ID])
        );
    }

    #[tokio::test]
    async fn error_envelope_with_exit_zero() {
        let fake = Fake::new(r#"echo '{"status":"error","error":"Calendar not found: X"}'"#);
        let runner = fake.runner();
        let err = runner
            .session()
            .await
            .list_events(&range())
            .await
            .unwrap_err();
        let EkctlError::Reported(message) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(message, "Calendar not found: X");
        assert_eq!(err.to_string(), "ekctl: Calendar not found: X");
    }

    #[tokio::test]
    async fn show_not_found_is_not_found() {
        let fake = Fake::printing("error.json");
        let runner = fake.runner();
        let err = runner
            .session()
            .await
            .show_event(&event_id("nonexistent-id"))
            .await
            .unwrap_err();
        let EkctlError::NotFound(message) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(message, "Event not found with ID: nonexistent-id");
    }

    #[tokio::test]
    async fn not_found_from_other_commands_is_reported() {
        let fake = Fake::printing("error.json");
        let runner = fake.runner();
        let err = runner
            .session()
            .await
            .delete_event(&event_id("nonexistent-id"))
            .await
            .unwrap_err();
        let EkctlError::Reported(_) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn other_show_error_is_reported() {
        let fake = Fake::new(r#"echo '{"status":"error","error":"Calendar access denied"}'"#);
        let runner = fake.runner();
        let err = runner
            .session()
            .await
            .show_event(&event_id(EVENT_ID))
            .await
            .unwrap_err();
        let EkctlError::Reported(message) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(message, "Calendar access denied");
    }

    #[tokio::test]
    async fn non_zero_exit_keeps_stderr_tail() {
        let fake = Fake::new(
            "i=0\nwhile [ $i -lt 100 ]; do printf 'noise-' >&2; i=$((i+1)); done\necho 'the real reason' >&2\nexit 3",
        );
        let runner = fake.runner();
        let err = runner.session().await.list_calendars().await.unwrap_err();
        let EkctlError::Exit {
            code,
            reason: stderr,
        } = &err
        else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(*code, Some(3));
        assert!(stderr.len() <= STDERR_TAIL, "{}", stderr.len());
        assert!(stderr.ends_with("noise-the real reason"), "{stderr}");
        assert!(err.to_string().starts_with("ekctl exited with code 3: "));
    }

    #[tokio::test]
    async fn non_zero_exit_without_stderr() {
        let fake = Fake::new("exit 1");
        let runner = fake.runner();
        let err = runner.session().await.list_calendars().await.unwrap_err();
        assert_eq!(err.to_string(), "ekctl exited with code 1");
    }

    #[tokio::test]
    async fn killed_by_a_signal() {
        let fake = Fake::new("kill -9 $$");
        let runner = fake.runner();
        let log = CallLog::default();
        let err = log
            .scope(async { runner.session().await.list_calendars().await.unwrap_err() })
            .await;
        let EkctlError::Exit {
            code: None,
            reason: stderr,
        } = &err
        else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(stderr, "");
        assert_eq!(err.to_string(), "ekctl was killed by a signal");
        let mut rendered = Vec::new();
        for call in log.calls() {
            rendered.push(call.to_string());
        }
        assert_eq!(rendered, vec!["list calendars=signalled"]);
    }

    #[tokio::test]
    async fn non_zero_exit_reports_the_stdout_envelope() {
        let fake = Fake::new(
            "echo '{\"status\":\"error\",\"error\":\"Permission denied for both Calendar and Reminders.\"}'\necho 'noise' >&2\nexit 2",
        );
        let runner = fake.runner();
        let session = runner.session().await;
        let err = session.list_calendars().await.unwrap_err();
        let EkctlError::Exit { code, reason } = &err else {
            panic!("expected an exit error, got {err:?}");
        };
        assert_eq!(*code, Some(2));
        assert_eq!(reason, "Permission denied for both Calendar and Reminders.");
        assert_eq!(
            err.to_string(),
            "ekctl exited with code 2: Permission denied for both Calendar and Reminders."
        );
    }

    #[tokio::test]
    async fn stderr_with_exit_zero_is_ignored() {
        let fake = Fake::new(&format!(
            "echo warning >&2\ncat '{}'",
            fixture("list_calendars.json").display()
        ));
        let runner = fake.runner();
        let calendars = runner.session().await.list_calendars().await.unwrap();
        assert_eq!(calendars.len(), 3);
    }

    #[tokio::test]
    async fn timeout_kills_the_child() {
        let fake = Fake::new("echo start >> \"$LOG\"\nsleep 1\necho end >> \"$LOG\"");
        let runner = fake.runner_with_timeout(Duration::from_millis(300));
        let err = runner.session().await.list_calendars().await.unwrap_err();
        let EkctlError::Timeout = err else {
            panic!("unexpected error: {err:?}");
        };
        time::sleep(Duration::from_millis(1500)).await;
        assert!(!fake.log().contains("end"), "{}", fake.log());
    }

    #[tokio::test]
    async fn missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(dir.path().join("ekctl"), Duration::from_secs(5));
        let err = runner.session().await.list_calendars().await.unwrap_err();
        let EkctlError::Spawn(source) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(source.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn stdout_over_the_cap() {
        let fake = Fake::new(&format!("exec head -c {} /dev/zero", STDOUT_CAP + 1));
        let runner = fake.runner();
        let err = runner.session().await.list_calendars().await.unwrap_err();
        let EkctlError::OutputTooLarge = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn stdout_over_the_cap_from_a_stuck_writer() {
        let fake = Fake::new(&format!(
            "head -c {} /dev/zero\nsleep 5",
            STDOUT_CAP + READ_CHUNK
        ));
        let runner = fake.runner();
        let started = time::Instant::now();
        let err = runner.session().await.list_calendars().await.unwrap_err();
        let EkctlError::OutputTooLarge = err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn output_that_is_not_json() {
        let fake = Fake::new("echo 'Error: something broke'");
        let runner = fake.runner();
        let err = runner.session().await.list_calendars().await.unwrap_err();
        let EkctlError::UnexpectedOutput = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn json_of_the_wrong_shape() {
        let fake = Fake::printing("free.json");
        let runner = fake.runner();
        let err = runner.session().await.list_calendars().await.unwrap_err();
        let EkctlError::UnexpectedOutput = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(err.to_string(), "unexpected ekctl output");
    }

    #[tokio::test]
    async fn error_envelope_without_message() {
        let fake = Fake::new(r#"echo '{"status":"error"}'"#);
        let runner = fake.runner();
        let err = runner.session().await.list_calendars().await.unwrap_err();
        let EkctlError::UnexpectedOutput = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn call_log_records_calls_made_in_scope() {
        let fake = Fake::new("exit 4");
        let runner = fake.runner();
        let log = CallLog::default();
        log.scope(async {
            runner.session().await.list_calendars().await.unwrap_err();
            runner
                .session()
                .await
                .show_event(&event_id(EVENT_ID))
                .await
                .unwrap_err();
        })
        .await;
        runner.session().await.list_calendars().await.unwrap_err();
        let calls = log.calls();
        assert_eq!(
            calls,
            vec![
                Call {
                    subcommand: Subcommand::ListCalendars,
                    outcome: CallOutcome::Exited(4),
                },
                Call {
                    subcommand: Subcommand::ShowEvent,
                    outcome: CallOutcome::Exited(4),
                },
            ]
        );
        assert_eq!(calls[0].to_string(), "list calendars=4");
    }

    #[tokio::test]
    async fn call_log_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Runner::new(dir.path().join("ekctl"), Duration::from_secs(5));
        let reported = Fake::printing("error.json");
        let reported_runner = reported.runner();
        let log = CallLog::default();
        log.scope(async {
            missing.session().await.list_calendars().await.unwrap_err();
            reported_runner
                .session()
                .await
                .show_event(&event_id("nonexistent-id"))
                .await
                .unwrap_err();
        })
        .await;
        let mut rendered = Vec::new();
        for call in log.calls() {
            rendered.push(call.to_string());
        }
        assert_eq!(rendered, vec!["list calendars=not started", "show event=0"]);
    }

    #[tokio::test]
    async fn concurrent_calls_never_overlap() {
        let fake = Fake::new(&format!(
            "echo start >> \"$LOG\"\nsleep 0.2\necho end >> \"$LOG\"\ncat '{}'",
            fixture("list_calendars.json").display()
        ));
        let runner = Arc::new(fake.runner());
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let runner = Arc::clone(&runner);
            tasks.push(tokio::spawn(async move {
                runner.session().await.list_calendars().await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(fake.log(), "start\nend\n".repeat(3));
    }

    #[tokio::test]
    async fn session_holds_the_lock_across_calls() {
        let fake = Fake::new(&format!(
            "echo \"$1 start\" >> \"$LOG\"\nsleep 0.1\necho \"$1 end\" >> \"$LOG\"\ncase \"$1\" in\n  show) cat '{}' ;;\n  delete) cat '{}' ;;\n  *) cat '{}' ;;\nesac",
            fixture("show_event.json").display(),
            fixture("delete_event.json").display(),
            fixture("list_calendars.json").display()
        ));
        let runner = Arc::new(fake.runner());
        let session = runner.session().await;
        let other = {
            let runner = Arc::clone(&runner);
            tokio::spawn(async move { runner.session().await.list_calendars().await })
        };
        let id = event_id(EVENT_ID);
        session.show_event(&id).await.unwrap();
        session.delete_event(&id).await.unwrap();
        drop(session);
        other.await.unwrap().unwrap();
        assert_eq!(
            fake.log(),
            "show start\nshow end\ndelete start\ndelete end\nlist start\nlist end\n"
        );
    }
}
