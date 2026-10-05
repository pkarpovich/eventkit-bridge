use crate::config::{CalendarId, Config, ListId};
use crate::ekctl::{EkctlError, Session};
use crate::model::{Access, Calendar, CalendarKind, EkCalendar, Event, EventId};
use crate::remindctl::{self, RemindctlError};
use crate::reminders_model::{List, RcList, RcReminder, ReminderId};

/// A request the security policy refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// A requested calendar is outside the readable set.
    #[error("calendar not readable: {0}")]
    CalendarNotReadable(CalendarId),
    /// A read names no calendar and the config makes none readable.
    #[error("no readable calendars configured")]
    NoReadableCalendars,
    /// A requested event lives in a calendar outside the readable set.
    #[error("event is not in a readable calendar")]
    EventNotReadable,
    /// A write was requested but the config names no write calendars.
    #[error("no write calendars configured")]
    NoWriteCalendar,
    /// A new event names a calendar outside the writable set.
    #[error("calendar not writable: {0}")]
    CalendarNotWritable(CalendarId),
    /// A write targets an event outside the writable calendars.
    #[error("event is not in a writable calendar")]
    NotInWriteCalendar,
    /// A write targets a recurring event; `ekctl` would change the series' first occurrence.
    #[error(
        "recurring events cannot be changed: ekctl would change the first occurrence of the series"
    )]
    RecurringEvent,
    /// A requested reminder list is outside the readable lists.
    #[error("list not readable: {0}")]
    ListNotReadable(ListId),
    /// A read names no list and the config makes none readable.
    #[error("no readable lists configured")]
    NoReadableLists,
    /// A requested reminder lives in a list outside the readable lists.
    #[error("reminder is not in a readable list")]
    ReminderNotReadable,
    /// A reminder write was requested but the config names no write lists.
    #[error("no write lists configured")]
    NoWriteList,
    /// A new reminder names a list outside the write lists.
    #[error("list not writable: {0}")]
    ListNotWritable(ListId),
    /// A write targets a reminder outside the write lists.
    #[error("reminder is not in a writable list")]
    NotInWriteList,
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

/// Why the reminder write guard did not allow a write.
#[derive(Debug, thiserror::Error)]
pub enum ReminderGuardError {
    /// The policy refuses the write.
    #[error(transparent)]
    Denied(#[from] PolicyError),
    /// `remindctl info` failed, including when the reminder does not exist.
    #[error(transparent)]
    Remindctl(#[from] RemindctlError),
}

/// Which calendars and reminder lists reads and writes may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    readable: Vec<CalendarId>,
    write: Vec<CalendarId>,
    readable_lists: Vec<ListId>,
    write_lists: Vec<ListId>,
}

impl Policy {
    /// The policy the config describes; every write calendar and write list is also readable.
    pub fn new(config: &Config) -> Self {
        Self {
            readable: config.readable_calendars(),
            write: config.write_calendars.clone(),
            readable_lists: config.readable_lists(),
            write_lists: config.write_lists.clone(),
        }
    }

    /// Whether reads may touch the calendar `id`.
    pub fn readable(&self, id: &CalendarId) -> bool {
        self.readable.contains(id)
    }

    /// Whether writes may touch the calendar `id`.
    pub fn writable(&self, id: &CalendarId) -> bool {
        self.write.contains(id)
    }

    /// Refuses a new event in `id` unless `id` is one of the write calendars.
    pub fn require_writable(&self, id: &CalendarId) -> Result<(), PolicyError> {
        if self.write.is_empty() {
            return Err(PolicyError::NoWriteCalendar);
        }
        if !self.writable(id) {
            return Err(PolicyError::CalendarNotWritable(id.clone()));
        }
        Ok(())
    }

    /// Every readable calendar, the set a read uses when the client names none.
    pub fn default_read_set(&self) -> Vec<CalendarId> {
        self.readable.clone()
    }

    /// Keeps the readable event calendars of `calendars`, marking the write calendars writable.
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
    /// `ids` is empty. Refuses the first id that is not readable, and an empty read set.
    pub fn require_readable(&self, ids: Vec<CalendarId>) -> Result<Vec<CalendarId>, PolicyError> {
        if ids.is_empty() {
            let all = self.default_read_set();
            if all.is_empty() {
                return Err(PolicyError::NoReadableCalendars);
            }
            return Ok(all);
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

    /// Refuses an event whose calendar is not readable.
    pub fn require_event_readable(&self, event: &Event) -> Result<(), PolicyError> {
        if !self.readable(&event.calendar.id) {
            return Err(PolicyError::EventNotReadable);
        }
        Ok(())
    }

    /// Shows the event `id` through `session` and returns it when it lives in a write calendar
    /// and is not recurring. The caller makes the write through the same session, so nothing
    /// runs between the check and the write.
    pub async fn guard_write(
        &self,
        session: &Session<'_>,
        id: &EventId,
    ) -> Result<Event, GuardError> {
        if self.write.is_empty() {
            return Err(PolicyError::NoWriteCalendar.into());
        }
        let event = session.show_event(id).await?;
        if !self.writable(&event.calendar.id) {
            return Err(PolicyError::NotInWriteCalendar.into());
        }
        match event.recurring {
            Some(false) => Ok(event),
            Some(true) => Err(PolicyError::RecurringEvent.into()),
            None => Err(PolicyError::RecurringEvent.into()),
        }
    }

    /// Whether any reminder list is readable; without one, reminders are off.
    pub fn any_readable_list(&self) -> bool {
        !self.readable_lists.is_empty()
    }

    /// Whether reads may touch the reminder list `id`.
    pub fn readable_list(&self, id: &ListId) -> bool {
        self.readable_lists.contains(id)
    }

    /// Whether writes may touch the reminder list `id`.
    pub fn writable_list(&self, id: &ListId) -> bool {
        self.write_lists.contains(id)
    }

    /// Keeps the readable lists of `lists`, marking the write lists writable.
    pub fn filter_lists(&self, lists: Vec<RcList>) -> Vec<List> {
        let mut filtered = Vec::new();
        for list in lists {
            if !self.readable_list(&list.id) {
                continue;
            }
            let access = if self.writable_list(&list.id) {
                Access::Writable
            } else {
                Access::ReadOnly
            };
            filtered.push(list.into_list(access));
        }
        filtered
    }

    /// The lists a read touches: `ids` without duplicates, or every readable list when `ids`
    /// is empty. Refuses the first id that is not readable, and an empty read set.
    pub fn require_readable_lists(&self, ids: Vec<ListId>) -> Result<Vec<ListId>, PolicyError> {
        if ids.is_empty() {
            if self.readable_lists.is_empty() {
                return Err(PolicyError::NoReadableLists);
            }
            return Ok(self.readable_lists.clone());
        }
        let mut required = Vec::new();
        for id in ids {
            if !self.readable_list(&id) {
                return Err(PolicyError::ListNotReadable(id));
            }
            if !required.contains(&id) {
                required.push(id);
            }
        }
        Ok(required)
    }

    /// Refuses a new reminder in `id` unless `id` is one of the write lists.
    pub fn require_writable_list(&self, id: &ListId) -> Result<(), PolicyError> {
        if self.write_lists.is_empty() {
            return Err(PolicyError::NoWriteList);
        }
        if !self.writable_list(id) {
            return Err(PolicyError::ListNotWritable(id.clone()));
        }
        Ok(())
    }

    /// Refuses a reminder whose list is not readable.
    pub fn require_reminder_readable(&self, reminder: &RcReminder) -> Result<(), PolicyError> {
        if !self.readable_list(&reminder.list_id) {
            return Err(PolicyError::ReminderNotReadable);
        }
        Ok(())
    }

    /// Runs `remindctl info` on `id` through `session` and returns the reminder when it lives in
    /// a write list. The caller makes the write through the same session, so nothing runs
    /// between the check and the write.
    pub async fn guard_reminder_write(
        &self,
        session: &remindctl::Session<'_>,
        id: &ReminderId,
    ) -> Result<RcReminder, ReminderGuardError> {
        if self.write_lists.is_empty() {
            return Err(PolicyError::NoWriteList.into());
        }
        let reminder = session.info(id).await?;
        if !self.writable_list(&reminder.list_id) {
            return Err(PolicyError::NotInWriteList.into());
        }
        Ok(reminder)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use super::*;
    use crate::fake_ekctl::{Fake, fixture, fixture_text};
    use crate::model::{EkCalendarList, EkEventEnvelope};
    use crate::subprocess::StoreLock;

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const REMINDERS_ID: &str = "2F8BCC68-AD77-B8A4-9218-37BF6271D47D";
    const OTHER_ID: &str = "11111111-2222-3333-4444-555555555555";
    const EVENT_ID: &str = "46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076";
    const REMINDER_ID: &str = "1B2C3D4E-5F6A-4B7C-9D8E-0F1A2B3C4D5E";

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
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\", \"{REMINDERS_ID}\"]\nwrite_calendars = [\"{WRITE_ID}\"]"
        ))
    }

    fn read_only() -> Policy {
        policy(&format!(
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\"]"
        ))
    }

    fn list_id(value: &str) -> ListId {
        ListId::parse(value.to_owned()).unwrap()
    }

    fn reminder_id() -> ReminderId {
        ReminderId::parse(REMINDER_ID).unwrap()
    }

    fn lists_read_and_write() -> Policy {
        policy(&format!(
            "listen = \"127.0.0.1:8790\"\nread_lists = [\"{READ_ID}\"]\nwrite_lists = [\"{WRITE_ID}\"]"
        ))
    }

    fn lists_read_only() -> Policy {
        policy(&format!(
            "listen = \"127.0.0.1:8790\"\nread_lists = [\"{READ_ID}\"]"
        ))
    }

    fn listed_lists() -> Vec<RcList> {
        serde_json::from_str(&fixture_text("remindctl_list.json")).unwrap()
    }

    fn info_in(list: &str) -> String {
        fixture_text("remindctl_info.json").replace(WRITE_ID, list)
    }

    fn reminder_in(list: &str) -> RcReminder {
        serde_json::from_str(&info_in(list)).unwrap()
    }

    fn listed_calendars() -> Vec<EkCalendar> {
        let EkCalendarList { calendars } =
            serde_json::from_str(include_str!("../fixtures/list_calendars.json")).unwrap();
        calendars
    }

    fn show_event_in(calendar: &str) -> String {
        let json = fs::read_to_string(fixture("show_event.json")).unwrap();
        json.replace(READ_ID, calendar).replace(
            r#""hasRecurrenceRules":true"#,
            r#""hasRecurrenceRules":false"#,
        )
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
        assert_eq!(policy.require_writable(&calendar_id(WRITE_ID)), Ok(()));
        assert_eq!(
            policy.require_writable(&calendar_id(READ_ID)),
            Err(PolicyError::CalendarNotWritable(calendar_id(READ_ID)))
        );
    }

    #[test]
    fn nothing_writable_without_write_calendar() {
        let policy = read_only();
        assert!(!policy.writable(&calendar_id(READ_ID)));
        assert!(!policy.readable(&calendar_id(WRITE_ID)));
        assert_eq!(
            policy.require_writable(&calendar_id(READ_ID)),
            Err(PolicyError::NoWriteCalendar)
        );
        assert_eq!(
            PolicyError::NoWriteCalendar.to_string(),
            "no write calendars configured"
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
    fn several_write_calendars_are_all_writable() {
        let policy = policy(&format!(
            "listen = \"127.0.0.1:8790\"\nwrite_calendars = [\"{READ_ID}\", \"{WRITE_ID}\"]"
        ));
        assert!(policy.writable(&calendar_id(READ_ID)));
        assert!(policy.writable(&calendar_id(WRITE_ID)));
        assert!(!policy.writable(&calendar_id(OTHER_ID)));
        let calendars = policy.filter_calendars(listed_calendars());
        assert_eq!(calendars.len(), 2);
        for calendar in calendars {
            assert!(calendar.writable, "{}", calendar.id);
        }
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
    fn empty_request_with_nothing_readable_is_refused() {
        let policy = policy("listen = \"127.0.0.1:8790\"");
        assert_eq!(
            policy.require_readable(Vec::new()),
            Err(PolicyError::NoReadableCalendars)
        );
        assert_eq!(
            policy.require_readable(vec![calendar_id(READ_ID)]),
            Err(PolicyError::CalendarNotReadable(calendar_id(READ_ID)))
        );
    }

    #[test]
    fn event_readability() {
        let EkEventEnvelope { event } = serde_json::from_str(&show_event_in(OTHER_ID)).unwrap();
        assert_eq!(
            read_and_write().require_event_readable(&Event::from(event)),
            Err(PolicyError::EventNotReadable)
        );
        let EkEventEnvelope { event } = serde_json::from_str(&show_event_in(WRITE_ID)).unwrap();
        let event = Event::from(event);
        assert_eq!(read_and_write().require_event_readable(&event), Ok(()));
        assert_eq!(
            read_only().require_event_readable(&event),
            Err(PolicyError::EventNotReadable)
        );
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
        assert_eq!(err.to_string(), "event is not in a writable calendar");
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

    #[test]
    fn lists_readable_and_writable() {
        let policy = lists_read_and_write();
        assert!(policy.readable_list(&list_id(READ_ID)));
        assert!(policy.readable_list(&list_id(WRITE_ID)));
        assert!(!policy.readable_list(&list_id(OTHER_ID)));
        assert!(policy.writable_list(&list_id(WRITE_ID)));
        assert!(!policy.writable_list(&list_id(READ_ID)));
        assert!(!policy.writable_list(&list_id(OTHER_ID)));
    }

    #[test]
    fn any_readable_list() {
        assert!(lists_read_and_write().any_readable_list());
        assert!(lists_read_only().any_readable_list());
        assert!(!read_and_write().any_readable_list());
    }

    #[test]
    fn calendars_and_lists_are_separate() {
        let policy = read_and_write();
        assert!(!policy.readable_list(&list_id(READ_ID)));
        assert!(!policy.writable_list(&list_id(WRITE_ID)));
        let policy = lists_read_and_write();
        assert!(!policy.readable(&calendar_id(READ_ID)));
        assert!(!policy.writable(&calendar_id(WRITE_ID)));
    }

    #[test]
    fn filter_lists_keeps_readable_and_marks_write_lists() {
        let lists = lists_read_and_write().filter_lists(listed_lists());
        assert_eq!(
            serde_json::to_value(lists).unwrap(),
            serde_json::json!([
                {"id": WRITE_ID, "title": "Shopping", "open": 4, "writable": true},
                {"id": READ_ID, "title": "Personal", "open": 2, "writable": false}
            ])
        );
    }

    #[test]
    fn filter_lists_drops_non_readable() {
        let lists = lists_read_only().filter_lists(listed_lists());
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].id, list_id(READ_ID));
        assert!(!lists[0].writable);
    }

    #[test]
    fn filter_lists_with_nothing_configured_is_empty() {
        assert!(read_and_write().filter_lists(listed_lists()).is_empty());
    }

    #[test]
    fn empty_list_request_means_all_readable_lists() {
        assert_eq!(
            lists_read_and_write().require_readable_lists(Vec::new()),
            Ok(vec![list_id(READ_ID), list_id(WRITE_ID)])
        );
    }

    #[test]
    fn requested_lists_kept_in_order_without_duplicates() {
        let required = lists_read_and_write()
            .require_readable_lists(vec![list_id(WRITE_ID), list_id(READ_ID), list_id(WRITE_ID)])
            .unwrap();
        assert_eq!(required, vec![list_id(WRITE_ID), list_id(READ_ID)]);
    }

    #[test]
    fn non_readable_list_refused() {
        let err = lists_read_and_write()
            .require_readable_lists(vec![list_id(READ_ID), list_id(OTHER_ID)])
            .unwrap_err();
        assert_eq!(err, PolicyError::ListNotReadable(list_id(OTHER_ID)));
        assert_eq!(err.to_string(), format!("list not readable: {OTHER_ID}"));
    }

    #[test]
    fn list_request_with_nothing_readable_is_refused() {
        let policy = read_and_write();
        assert_eq!(
            policy.require_readable_lists(Vec::new()),
            Err(PolicyError::NoReadableLists)
        );
        assert_eq!(
            policy.require_readable_lists(vec![list_id(READ_ID)]),
            Err(PolicyError::ListNotReadable(list_id(READ_ID)))
        );
        assert_eq!(
            PolicyError::NoReadableLists.to_string(),
            "no readable lists configured"
        );
    }

    #[test]
    fn new_reminder_needs_a_write_list() {
        let policy = lists_read_and_write();
        assert_eq!(policy.require_writable_list(&list_id(WRITE_ID)), Ok(()));
        let err = policy.require_writable_list(&list_id(READ_ID)).unwrap_err();
        assert_eq!(err, PolicyError::ListNotWritable(list_id(READ_ID)));
        assert_eq!(err.to_string(), format!("list not writable: {READ_ID}"));
        assert_eq!(
            lists_read_only().require_writable_list(&list_id(READ_ID)),
            Err(PolicyError::NoWriteList)
        );
        assert_eq!(
            PolicyError::NoWriteList.to_string(),
            "no write lists configured"
        );
    }

    #[test]
    fn reminder_readability() {
        let policy = lists_read_and_write();
        assert_eq!(
            policy.require_reminder_readable(&reminder_in(WRITE_ID)),
            Ok(())
        );
        assert_eq!(
            policy.require_reminder_readable(&reminder_in(READ_ID)),
            Ok(())
        );
        let err = policy
            .require_reminder_readable(&reminder_in(OTHER_ID))
            .unwrap_err();
        assert_eq!(err, PolicyError::ReminderNotReadable);
        assert_eq!(err.to_string(), "reminder is not in a readable list");
        assert_eq!(
            lists_read_only().require_reminder_readable(&reminder_in(WRITE_ID)),
            Err(PolicyError::ReminderNotReadable)
        );
    }

    #[tokio::test]
    async fn reminder_guard_allows_a_write_list() {
        let fake = Fake::recording_json(&info_in(WRITE_ID));
        let runner = fake.remindctl_runner(StoreLock::default());
        let session = runner.session().await;
        let reminder = lists_read_and_write()
            .guard_reminder_write(&session, &reminder_id())
            .await
            .unwrap();
        assert_eq!(reminder.id, reminder_id());
        assert_eq!(reminder.list_id, list_id(WRITE_ID));
        assert_eq!(
            fake.recorded_args(),
            vec!["info", "--json", "--no-input", "--", REMINDER_ID]
        );
    }

    #[tokio::test]
    async fn reminder_guard_refuses_a_read_list() {
        let fake = Fake::recording_json(&info_in(READ_ID));
        let runner = fake.remindctl_runner(StoreLock::default());
        let session = runner.session().await;
        let err = lists_read_and_write()
            .guard_reminder_write(&session, &reminder_id())
            .await
            .unwrap_err();
        let ReminderGuardError::Denied(PolicyError::NotInWriteList) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(err.to_string(), "reminder is not in a writable list");
    }

    #[tokio::test]
    async fn reminder_guard_refuses_an_unconfigured_list() {
        let fake = Fake::recording_json(&info_in(OTHER_ID));
        let runner = fake.remindctl_runner(StoreLock::default());
        let session = runner.session().await;
        let err = lists_read_and_write()
            .guard_reminder_write(&session, &reminder_id())
            .await
            .unwrap_err();
        let ReminderGuardError::Denied(PolicyError::NotInWriteList) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn reminder_guard_passes_through_not_found() {
        let fake = Fake::new(&format!(
            "echo 'Reminder not found: \"{REMINDER_ID}\".' >&2\nexit 1"
        ));
        let runner = fake.remindctl_runner(StoreLock::default());
        let session = runner.session().await;
        let err = lists_read_and_write()
            .guard_reminder_write(&session, &reminder_id())
            .await
            .unwrap_err();
        let ReminderGuardError::Remindctl(RemindctlError::NotFound(message)) = &err else {
            panic!("unexpected error: {err:?}");
        };
        let expected = format!("Reminder not found: \"{REMINDER_ID}\".");
        assert_eq!(message, &expected);
        assert_eq!(err.to_string(), expected);
    }

    #[tokio::test]
    async fn reminder_guard_without_write_list_never_runs_remindctl() {
        let fake = Fake::recording_json(&info_in(READ_ID));
        let runner = fake.remindctl_runner(StoreLock::default());
        let session = runner.session().await;
        let err = lists_read_only()
            .guard_reminder_write(&session, &reminder_id())
            .await
            .unwrap_err();
        let ReminderGuardError::Denied(PolicyError::NoWriteList) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(fake.recorded_args().is_empty());
    }

    #[tokio::test]
    async fn reminder_guard_and_write_share_one_session() {
        let fake = Fake::new(&format!(
            "echo \"$1 start\" >> \"$LOG\"\nsleep 0.1\necho \"$1 end\" >> \"$LOG\"\ncase \"$1\" in\n  info) cat <<'JSON'\n{}\nJSON\n  ;;\n  delete) cat '{}' ;;\n  *) cat '{}' ;;\nesac",
            info_in(WRITE_ID),
            fixture("remindctl_delete.json").display(),
            fixture("list_calendars.json").display()
        ));
        let calendars = Arc::new(fake.runner());
        let reminders = fake.remindctl_runner(calendars.lock());
        let policy = lists_read_and_write();
        let session = reminders.session().await;
        let other = {
            let calendars = Arc::clone(&calendars);
            tokio::spawn(async move { calendars.session().await.list_calendars().await })
        };
        let id = reminder_id();
        policy.guard_reminder_write(&session, &id).await.unwrap();
        session.delete(&id).await.unwrap();
        drop(session);
        other.await.unwrap().unwrap();
        assert_eq!(
            fake.log(),
            "info start\ninfo end\ndelete start\ndelete end\nlist start\nlist end\n"
        );
    }
}
