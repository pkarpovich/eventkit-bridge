use crate::config::{CalendarId, Config};
use crate::ekctl::{EkctlError, Session};
use crate::model::{Access, Calendar, CalendarKind, EkCalendar, Event, EventId};

/// A request the security policy refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// A requested calendar is outside the readable set.
    #[error("calendar not readable: {0}")]
    CalendarNotReadable(CalendarId),
    /// A write was requested but the config names no write calendar.
    #[error("no write calendar configured")]
    NoWriteCalendar,
    /// A write targets an event outside the write calendar.
    #[error("event is not in the write calendar")]
    NotInWriteCalendar,
}

/// Why the write guard did not allow a write.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    /// The policy refuses the write.
    #[error(transparent)]
    Denied(#[from] PolicyError),
    /// `ekctl show event` failed, including when the event does not exist.
    #[error(transparent)]
    Ekctl(#[from] EkctlError),
}

/// Which calendars reads and writes may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    readable: Vec<CalendarId>,
    write: Option<CalendarId>,
}

impl Policy {
    /// The policy the config describes; the write calendar is always readable.
    pub fn new(config: &Config) -> Self {
        Self {
            readable: config.readable_calendars(),
            write: config.write_calendar.clone(),
        }
    }

    /// Whether reads may touch the calendar `id`.
    pub fn readable(&self, id: &CalendarId) -> bool {
        self.readable.contains(id)
    }

    /// Whether writes may touch the calendar `id`: true for the write calendar alone.
    pub fn writable(&self, id: &CalendarId) -> bool {
        self.write.as_ref() == Some(id)
    }

    /// The write calendar, or [`PolicyError::NoWriteCalendar`] when none is configured.
    pub fn write_calendar(&self) -> Result<&CalendarId, PolicyError> {
        let Some(write) = &self.write else {
            return Err(PolicyError::NoWriteCalendar);
        };
        Ok(write)
    }

    /// Every readable calendar, the set a read uses when the client names none.
    pub fn default_read_set(&self) -> Vec<CalendarId> {
        self.readable.clone()
    }

    /// Keeps the readable event calendars of `calendars`, marking the write calendar writable.
    pub fn filter_calendars(&self, calendars: Vec<EkCalendar>) -> Vec<Calendar> {
        let mut filtered = Vec::new();
        for calendar in calendars {
            let is_event = match calendar.kind {
                CalendarKind::Event => true,
                CalendarKind::Reminder => false,
                CalendarKind::Other => false,
            };
            if !is_event || !self.readable(&calendar.id) {
                continue;
            }
            let access = if self.writable(&calendar.id) {
                Access::Writable
            } else {
                Access::ReadOnly
            };
            filtered.push(calendar.into_calendar(access));
        }
        filtered
    }

    /// The calendars a read touches: `ids` without duplicates, or the default read set when
    /// `ids` is empty. Refuses the first id that is not readable.
    pub fn require_readable(&self, ids: Vec<CalendarId>) -> Result<Vec<CalendarId>, PolicyError> {
        if ids.is_empty() {
            return Ok(self.default_read_set());
        }
        let mut required = Vec::new();
        for id in ids {
            if !self.readable(&id) {
                return Err(PolicyError::CalendarNotReadable(id));
            }
            if !required.contains(&id) {
                required.push(id);
            }
        }
        Ok(required)
    }

    /// Shows the event `id` through `session` and returns it when it lives in the write
    /// calendar. The caller makes the write through the same session, so nothing runs between
    /// the check and the write.
    pub async fn guard_write(
        &self,
        session: &Session<'_>,
        id: &EventId,
    ) -> Result<Event, GuardError> {
        let write = self.write_calendar()?;
        let event = session.show_event(id).await?;
        if &event.calendar.id != write {
            return Err(PolicyError::NotInWriteCalendar.into());
        }
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use super::*;
    use crate::fake_ekctl::{Fake, fixture};
    use crate::model::EkCalendarList;

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const REMINDERS_ID: &str = "2F8BCC68-AD77-B8A4-9218-37BF6271D47D";
    const OTHER_ID: &str = "11111111-2222-3333-4444-555555555555";
    const EVENT_ID: &str = "46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076";

    fn calendar_id(value: &str) -> CalendarId {
        CalendarId::parse(value.to_owned()).unwrap()
    }

    fn event_id(value: &str) -> EventId {
        EventId::parse(value).unwrap()
    }

    fn policy(toml: &str) -> Policy {
        Policy::new(&Config::from_toml(toml).unwrap())
    }

    fn read_and_write() -> Policy {
        policy(&format!(
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\", \"{REMINDERS_ID}\"]\nwrite_calendar = \"{WRITE_ID}\""
        ))
    }

    fn read_only() -> Policy {
        policy(&format!(
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\"]"
        ))
    }

    fn listed_calendars() -> Vec<EkCalendar> {
        let EkCalendarList { calendars } =
            serde_json::from_str(include_str!("../fixtures/list_calendars.json")).unwrap();
        calendars
    }

    fn show_event_in(calendar: &str) -> String {
        let json = fs::read_to_string(fixture("show_event.json")).unwrap();
        json.replace(READ_ID, calendar)
    }

    #[test]
    fn readable_and_writable() {
        let policy = read_and_write();
        assert!(policy.readable(&calendar_id(READ_ID)));
        assert!(policy.readable(&calendar_id(WRITE_ID)));
        assert!(!policy.readable(&calendar_id(OTHER_ID)));
        assert!(policy.writable(&calendar_id(WRITE_ID)));
        assert!(!policy.writable(&calendar_id(READ_ID)));
        assert!(!policy.writable(&calendar_id(OTHER_ID)));
        assert_eq!(policy.write_calendar(), Ok(&calendar_id(WRITE_ID)));
    }

    #[test]
    fn nothing_writable_without_write_calendar() {
        let policy = read_only();
        assert!(!policy.writable(&calendar_id(READ_ID)));
        assert!(!policy.readable(&calendar_id(WRITE_ID)));
        assert_eq!(policy.write_calendar(), Err(PolicyError::NoWriteCalendar));
        assert_eq!(
            PolicyError::NoWriteCalendar.to_string(),
            "no write calendar configured"
        );
    }

    #[test]
    fn filter_drops_reminder_lists_and_marks_write_calendar() {
        let calendars = read_and_write().filter_calendars(listed_calendars());
        assert_eq!(
            serde_json::to_value(calendars).unwrap(),
            serde_json::json!([
                {"id": READ_ID, "title": "Calendar", "source": "work@example.com", "color": "#0088FF", "writable": false},
                {"id": WRITE_ID, "title": "Agent", "source": "iCloud", "color": "#34C759", "writable": true}
            ])
        );
    }

    #[test]
    fn filter_drops_non_readable() {
        let calendars = read_only().filter_calendars(listed_calendars());
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].id, calendar_id(READ_ID));
        assert!(!calendars[0].writable);
    }

    #[test]
    fn filter_with_nothing_configured_is_empty() {
        let policy = policy("listen = \"127.0.0.1:8790\"");
        assert!(policy.filter_calendars(listed_calendars()).is_empty());
        assert!(policy.default_read_set().is_empty());
    }

    #[test]
    fn non_readable_id_refused() {
        let err = read_and_write()
            .require_readable(vec![calendar_id(READ_ID), calendar_id(OTHER_ID)])
            .unwrap_err();
        assert_eq!(err, PolicyError::CalendarNotReadable(calendar_id(OTHER_ID)));
        assert_eq!(
            err.to_string(),
            format!("calendar not readable: {OTHER_ID}")
        );
    }

    #[test]
    fn empty_request_means_all_readable() {
        let policy = read_and_write();
        let expected = vec![
            calendar_id(READ_ID),
            calendar_id(REMINDERS_ID),
            calendar_id(WRITE_ID),
        ];
        assert_eq!(policy.default_read_set(), expected);
        assert_eq!(policy.require_readable(Vec::new()), Ok(expected));
    }

    #[test]
    fn requested_ids_kept_in_order_without_duplicates() {
        let required = read_and_write()
            .require_readable(vec![
                calendar_id(WRITE_ID),
                calendar_id(READ_ID),
                calendar_id(WRITE_ID),
            ])
            .unwrap();
        assert_eq!(required, vec![calendar_id(WRITE_ID), calendar_id(READ_ID)]);
    }

    #[tokio::test]
    async fn guard_allows_the_write_calendar() {
        let fake = Fake::recording_json(&show_event_in(WRITE_ID));
        let runner = fake.runner();
        let session = runner.session().await;
        let event = read_and_write()
            .guard_write(&session, &event_id(EVENT_ID))
            .await
            .unwrap();
        assert_eq!(event.id, event_id(EVENT_ID));
        assert_eq!(event.calendar.id, calendar_id(WRITE_ID));
        assert_eq!(event.end.as_str(), "2026-10-05T11:30:00+02:00");
        assert_eq!(fake.recorded_args(), vec!["show", "event", "--", EVENT_ID]);
    }

    #[tokio::test]
    async fn guard_refuses_a_user_calendar() {
        let fake = Fake::printing("show_event.json");
        let runner = fake.runner();
        let session = runner.session().await;
        let err = read_and_write()
            .guard_write(&session, &event_id(EVENT_ID))
            .await
            .unwrap_err();
        let GuardError::Denied(PolicyError::NotInWriteCalendar) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(err.to_string(), "event is not in the write calendar");
    }

    #[tokio::test]
    async fn guard_refuses_an_unreadable_calendar() {
        let fake = Fake::recording_json(&show_event_in(OTHER_ID));
        let runner = fake.runner();
        let session = runner.session().await;
        let err = read_and_write()
            .guard_write(&session, &event_id(EVENT_ID))
            .await
            .unwrap_err();
        let GuardError::Denied(PolicyError::NotInWriteCalendar) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn guard_passes_through_not_found() {
        let fake = Fake::printing("error.json");
        let runner = fake.runner();
        let session = runner.session().await;
        let err = read_and_write()
            .guard_write(&session, &event_id("nonexistent-id"))
            .await
            .unwrap_err();
        let GuardError::Ekctl(EkctlError::NotFound(message)) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(message, "Event not found with ID: nonexistent-id");
        assert_eq!(err.to_string(), "Event not found with ID: nonexistent-id");
    }

    #[tokio::test]
    async fn guard_without_write_calendar_never_runs_ekctl() {
        let fake = Fake::recording_json(&show_event_in(READ_ID));
        let runner = fake.runner();
        let session = runner.session().await;
        let err = read_only()
            .guard_write(&session, &event_id(EVENT_ID))
            .await
            .unwrap_err();
        let GuardError::Denied(PolicyError::NoWriteCalendar) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(fake.recorded_args().is_empty());
    }

    #[tokio::test]
    async fn guard_and_write_share_one_session() {
        let fake = Fake::new(&format!(
            "echo \"$1 start\" >> \"$LOG\"\nsleep 0.1\necho \"$1 end\" >> \"$LOG\"\ncase \"$1\" in\n  show) cat <<'JSON'\n{}\nJSON\n  ;;\n  delete) cat '{}' ;;\n  *) cat '{}' ;;\nesac",
            show_event_in(WRITE_ID),
            fixture("delete_event.json").display(),
            fixture("list_calendars.json").display()
        ));
        let runner = Arc::new(fake.runner());
        let policy = read_and_write();
        let session = runner.session().await;
        let other = {
            let runner = Arc::clone(&runner);
            tokio::spawn(async move { runner.session().await.list_calendars().await })
        };
        let id = event_id(EVENT_ID);
        policy.guard_write(&session, &id).await.unwrap();
        session.delete_event(&id).await.unwrap();
        drop(session);
        other.await.unwrap().unwrap();
        assert_eq!(
            fake.log(),
            "show start\nshow end\ndelete start\ndelete end\nlist start\nlist end\n"
        );
    }
}
