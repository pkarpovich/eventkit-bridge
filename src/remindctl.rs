use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::sync::MutexGuard;

use crate::config::{ListId, Place};
use crate::reminders_model::{
    Due, Priority, Proximity, RcDeleted, RcList, RcReminder, RcStatus, ReminderId, Repeat,
};
use crate::subprocess::{self, CallOutcome, Output, RunError, StoreLock};

const REMINDER_NOT_FOUND: &str = "Reminder not found";
const LIST_NOT_FOUND: &str = "List not found";

/// Why a `remindctl` invocation produced no usable result.
#[derive(Debug, thiserror::Error)]
pub enum RemindctlError {
    /// `remindctl` could not be started.
    #[error("remindctl could not start: {0}")]
    Spawn(#[source] std::io::Error),
    /// Reading `remindctl`'s output or waiting for it failed.
    #[error("remindctl i/o failed: {0}")]
    Io(#[source] std::io::Error),
    /// `remindctl` ran past its deadline and was killed.
    #[error("remindctl timed out")]
    Timeout,
    /// `remindctl` wrote more than the stdout cap and was killed.
    #[error("output too large")]
    OutputTooLarge,
    /// `remindctl` reported that the reminder does not exist.
    #[error("{0}")]
    NotFound(String),
    /// `remindctl` reported that a configured list does not exist, so the config is stale.
    #[error("remindctl: {0}")]
    ListNotFound(String),
    /// `remindctl` exited unsuccessfully for another reason.
    #[error("{}", exit_message(*.code, .reason))]
    Exit {
        /// The exit code, `None` when a signal ended the process.
        code: Option<i32>,
        /// The last bytes of stderr.
        reason: String,
    },
    /// `remindctl`'s stdout was not the JSON shape the command produces.
    #[error("unexpected remindctl output")]
    UnexpectedOutput,
}

impl From<RunError> for RemindctlError {
    fn from(err: RunError) -> Self {
        match err {
            RunError::Spawn(source) => RemindctlError::Spawn(source),
            RunError::Io(source) => RemindctlError::Io(source),
            RunError::Timeout => RemindctlError::Timeout,
            RunError::OutputTooLarge => RemindctlError::OutputTooLarge,
        }
    }
}

fn exit_message(code: Option<i32>, reason: &str) -> String {
    let status = match code {
        Some(code) => format!("remindctl exited with code {code}"),
        None => "remindctl was killed by a signal".to_owned(),
    };
    if reason.is_empty() {
        return status;
    }
    format!("{status}: {reason}")
}

/// The `remindctl` commands the bridge runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `list`.
    List,
    /// `show`.
    Show,
    /// `info`.
    Info,
    /// `add`.
    Add,
    /// `edit`.
    Edit,
    /// `delete`.
    Delete,
    /// `status`.
    Status,
}

impl Command {
    /// The command as `remindctl` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Command::List => "list",
            Command::Show => "show",
            Command::Info => "info",
            Command::Add => "add",
            Command::Edit => "edit",
            Command::Delete => "delete",
            Command::Status => "status",
        }
    }
}

/// One `remindctl` invocation, as the request log reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    /// The command that ran.
    pub command: Command,
    /// How it ended.
    pub outcome: CallOutcome,
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Call { command, outcome } = self;
        write!(f, "{}={outcome}", command.as_str())
    }
}

tokio::task_local! {
    static CALLS: CallLog;
}

/// Collects the `remindctl` invocations made by a future run through [`CallLog::scope`].
#[derive(Debug, Clone, Default)]
pub struct CallLog(Arc<std::sync::Mutex<Vec<Call>>>);

impl CallLog {
    /// Runs `future`, recording every `remindctl` invocation it makes on this task.
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

    fn record(command: Command, outcome: CallOutcome) {
        let recorded = CALLS.try_with(|log| {
            log.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Call { command, outcome });
        });
        recorded.ok();
    }
}

fn outcome(result: &Result<Output, RunError>) -> CallOutcome {
    match result {
        Ok(Output {
            stdout: _,
            stderr: _,
            status,
        }) => match status.code() {
            Some(code) => CallOutcome::Exited(code),
            None => CallOutcome::Signalled,
        },
        Err(RunError::Spawn(_)) => CallOutcome::NotStarted,
        Err(RunError::Timeout) => CallOutcome::TimedOut,
        Err(RunError::Io(_) | RunError::OutputTooLarge) => CallOutcome::Killed,
    }
}

/// Which reminders `show` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowFilter {
    /// Incomplete reminders.
    Open,
    /// Completed reminders.
    Completed,
    /// Every reminder.
    All,
}

impl ShowFilter {
    fn as_str(self) -> &'static str {
        match self {
            ShowFilter::Open => "open",
            ShowFilter::Completed => "completed",
            ShowFilter::All => "all",
        }
    }
}

/// A location trigger for a new reminder, at a configured place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    /// The place whose address and radius the trigger uses.
    pub place: Place,
    /// When the trigger fires.
    pub proximity: Proximity,
}

/// A reminder to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReminder {
    /// The list to create it in.
    pub list: ListId,
    /// The title.
    pub title: String,
    /// The notes.
    pub notes: Option<String>,
    /// The due date.
    pub due: Option<Due>,
    /// The repeat rule.
    pub repeat: Option<Repeat>,
    /// The priority.
    pub priority: Option<Priority>,
    /// The location trigger.
    pub location: Option<Trigger>,
}

/// A change to a field that can also be removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change<T> {
    /// Set the field to this value.
    Set(T),
    /// Remove the field.
    Clear,
}

/// Whether to mark a reminder completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Mark it completed.
    Complete,
    /// Mark it incomplete.
    Incomplete,
}

/// The fields of a reminder to change; `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReminderChanges {
    /// The new title.
    pub title: Option<String>,
    /// The new notes.
    pub notes: Option<String>,
    /// The new or removed due date.
    pub due: Option<Change<Due>>,
    /// The new or removed repeat rule.
    pub repeat: Option<Change<Repeat>>,
    /// The new priority.
    pub priority: Option<Priority>,
    /// The new completion state.
    pub completion: Option<Completion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    command: Command,
    args: Vec<String>,
}

impl Invocation {
    fn new(command: Command, positional: Option<&str>) -> Self {
        let mut args = vec![command.as_str().to_owned()];
        if let Some(positional) = positional {
            args.push(positional.to_owned());
        }
        args.push("--json".to_owned());
        args.push("--no-input".to_owned());
        Self { command, args }
    }

    fn flag(&mut self, name: &str) {
        self.args.push(format!("--{name}"));
    }

    fn option(&mut self, name: &str, value: &str) {
        self.args.push(format!("--{name}={value}"));
    }

    fn optional(&mut self, name: &str, value: Option<&str>) {
        if let Some(value) = value {
            self.option(name, value);
        }
    }

    fn reminder_id(&mut self, id: &ReminderId) {
        self.args.push("--".to_owned());
        self.args.push(id.as_str().to_owned());
    }
}

fn list() -> Invocation {
    Invocation::new(Command::List, None)
}

fn show(filter: ShowFilter, list: &ListId) -> Invocation {
    let mut invocation = Invocation::new(Command::Show, Some(filter.as_str()));
    invocation.option("list-id", list.as_str());
    invocation
}

fn info(id: &ReminderId) -> Invocation {
    let mut invocation = Invocation::new(Command::Info, None);
    invocation.reminder_id(id);
    invocation
}

fn add(reminder: &NewReminder) -> Invocation {
    let NewReminder {
        list,
        title,
        notes,
        due,
        repeat,
        priority,
        location,
    } = reminder;
    let mut invocation = Invocation::new(Command::Add, None);
    invocation.option("title", title);
    invocation.option("list-id", list.as_str());
    invocation.optional("notes", notes.as_deref());
    invocation.optional("due", due.map(|due| due.to_string()).as_deref());
    invocation.optional("repeat", repeat.map(Repeat::as_str));
    invocation.optional("priority", priority.map(Priority::as_str));
    if let Some(trigger) = location {
        trigger_options(&mut invocation, trigger);
    }
    invocation
}

fn trigger_options(invocation: &mut Invocation, trigger: &Trigger) {
    let Trigger { place, proximity } = trigger;
    let Place {
        name: _,
        address,
        radius,
    } = place;
    invocation.option("location", address.as_str());
    invocation.option("radius", &radius.to_string());
    match proximity {
        Proximity::Arriving => {}
        Proximity::Leaving => invocation.flag("leaving"),
    }
}

fn edit(id: &ReminderId, changes: &ReminderChanges) -> Invocation {
    let ReminderChanges {
        title,
        notes,
        due,
        repeat,
        priority,
        completion,
    } = changes;
    let mut invocation = Invocation::new(Command::Edit, None);
    invocation.optional("title", title.as_deref());
    invocation.optional("notes", notes.as_deref());
    match due {
        None => {}
        Some(Change::Set(due @ Due::At(_))) => {
            invocation.option("due", &due.to_string());
            invocation.option("alarm", &due.to_string());
        }
        Some(Change::Set(due @ Due::Day(_))) => {
            invocation.option("due", &due.to_string());
            invocation.flag("clear-alarm");
        }
        Some(Change::Clear) => {
            invocation.flag("clear-due");
            invocation.flag("clear-alarm");
        }
    }
    match repeat {
        None => {}
        Some(Change::Set(repeat)) => invocation.option("repeat", repeat.as_str()),
        Some(Change::Clear) => invocation.flag("no-repeat"),
    }
    invocation.optional("priority", priority.map(Priority::as_str));
    match completion {
        None => {}
        Some(Completion::Complete) => invocation.flag("complete"),
        Some(Completion::Incomplete) => invocation.flag("incomplete"),
    }
    invocation.reminder_id(id);
    invocation
}

fn redact_address(err: RemindctlError, place: &Place) -> RemindctlError {
    let Place {
        name,
        address,
        radius: _,
    } = place;
    match err {
        RemindctlError::Exit { code, reason } => RemindctlError::Exit {
            code,
            reason: reason.replace(address.as_str(), &format!("place {name}")),
        },
        err @ (RemindctlError::Spawn(_)
        | RemindctlError::Io(_)
        | RemindctlError::Timeout
        | RemindctlError::OutputTooLarge
        | RemindctlError::NotFound(_)
        | RemindctlError::ListNotFound(_)
        | RemindctlError::UnexpectedOutput) => err,
    }
}

fn delete(id: &ReminderId) -> Invocation {
    let mut invocation = Invocation::new(Command::Delete, None);
    invocation.flag("force");
    invocation.reminder_id(id);
    invocation
}

fn status() -> Invocation {
    Invocation::new(Command::Status, None)
}

/// Runs `remindctl`, one invocation at a time, under the lock the `ekctl` runner also holds.
#[derive(Debug)]
pub struct Runner {
    program: PathBuf,
    timeout: Duration,
    lock: StoreLock,
}

/// Exclusive use of the EventKit store; every call made through one session runs under the same lock.
#[derive(Debug)]
pub struct Session<'a> {
    runner: &'a Runner,
    _guard: MutexGuard<'a, ()>,
}

impl Runner {
    /// A runner for the `remindctl` at `program`, killing any invocation that outlives `timeout`.
    pub fn new(program: PathBuf, timeout: Duration, lock: StoreLock) -> Self {
        Self {
            program,
            timeout,
            lock,
        }
    }

    /// Waits for every other session, of either runner, to end and starts a new one.
    pub async fn session(&self) -> Session<'_> {
        let guard = self.lock.acquire().await;
        Session {
            runner: self,
            _guard: guard,
        }
    }

    async fn run(&self, invocation: &Invocation) -> Result<Vec<u8>, RemindctlError> {
        let result = subprocess::run(&self.program, &invocation.args, self.timeout).await;
        CallLog::record(invocation.command, outcome(&result));
        let Output {
            stdout,
            stderr,
            status,
        } = result?;
        if status.success() {
            return Ok(stdout);
        }
        let reason = String::from_utf8_lossy(&stderr).trim().to_owned();
        if reason.starts_with(REMINDER_NOT_FOUND) {
            return Err(RemindctlError::NotFound(reason));
        }
        if reason.starts_with(LIST_NOT_FOUND) {
            return Err(RemindctlError::ListNotFound(reason));
        }
        Err(RemindctlError::Exit {
            code: status.code(),
            reason,
        })
    }
}

impl Session<'_> {
    /// Every reminder list `remindctl` can see.
    pub async fn list(&self) -> Result<Vec<RcList>, RemindctlError> {
        self.execute(&list()).await
    }

    /// The reminders of `list` matching `filter`, in `remindctl`'s order.
    pub async fn show(
        &self,
        filter: ShowFilter,
        list: &ListId,
    ) -> Result<Vec<RcReminder>, RemindctlError> {
        self.execute(&show(filter, list)).await
    }

    /// One reminder; [`RemindctlError::NotFound`] when it does not exist.
    pub async fn info(&self, id: &ReminderId) -> Result<RcReminder, RemindctlError> {
        self.execute(&info(id)).await
    }

    /// Creates `reminder` and returns it as `remindctl` stored it. A failure never carries the
    /// trigger's address: `remindctl` names it when geocoding fails, so it is replaced by the
    /// place name.
    pub async fn add(&self, reminder: &NewReminder) -> Result<RcReminder, RemindctlError> {
        let result = self.execute(&add(reminder)).await;
        let Some(Trigger {
            place,
            proximity: _,
        }) = &reminder.location
        else {
            return result;
        };
        result.map_err(|err| redact_address(err, place))
    }

    /// Applies `changes` to the reminder `id` and returns it.
    pub async fn edit(
        &self,
        id: &ReminderId,
        changes: &ReminderChanges,
    ) -> Result<RcReminder, RemindctlError> {
        self.execute(&edit(id, changes)).await
    }

    /// Deletes the reminder `id`.
    pub async fn delete(&self, id: &ReminderId) -> Result<(), RemindctlError> {
        let RcDeleted { deleted: _ } = self.execute(&delete(id)).await?;
        Ok(())
    }

    /// Whether the app may use reminders.
    pub async fn status(&self) -> Result<RcStatus, RemindctlError> {
        self.execute(&status()).await
    }

    async fn execute<T: DeserializeOwned>(
        &self,
        invocation: &Invocation,
    ) -> Result<T, RemindctlError> {
        let stdout = self.runner.run(invocation).await?;
        serde_json::from_slice(&stdout).map_err(|_| RemindctlError::UnexpectedOutput)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, NaiveDate};
    use tokio::time;

    use super::*;
    use crate::config::Config;
    use crate::fake_ekctl::{Fake, fixture};
    use crate::subprocess::{STDERR_TAIL, STDOUT_CAP};

    const WRITE_LIST: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const REMINDER: &str = "1B2C3D4E-5F6A-4B7C-9D8E-0F1A2B3C4D5E";
    const SHOP_ADDRESS: &str = "1 Example Street, Exampletown";

    fn list_id() -> ListId {
        ListId::parse(WRITE_LIST.to_owned()).unwrap()
    }

    fn reminder_id() -> ReminderId {
        ReminderId::parse(REMINDER).unwrap()
    }

    fn strings(values: &[&str]) -> Vec<String> {
        let mut owned = Vec::new();
        for value in values {
            owned.push((*value).to_owned());
        }
        owned
    }

    fn shop() -> Place {
        let config = Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\n[places]\nshop = {{ address = \"{SHOP_ADDRESS}\", radius = 150 }}\n"
        ))
        .unwrap();
        config.places[0].clone()
    }

    fn new_reminder() -> NewReminder {
        NewReminder {
            list: list_id(),
            title: "Eggs".to_owned(),
            notes: None,
            due: None,
            repeat: None,
            priority: None,
            location: None,
        }
    }

    #[test]
    fn list_argv() {
        assert_eq!(list().args, strings(&["list", "--json", "--no-input"]));
    }

    #[test]
    fn show_argv() {
        assert_eq!(
            show(ShowFilter::Open, &list_id()).args,
            strings(&[
                "show",
                "open",
                "--json",
                "--no-input",
                &format!("--list-id={WRITE_LIST}"),
            ])
        );
        assert_eq!(show(ShowFilter::Completed, &list_id()).args[1], "completed");
        assert_eq!(show(ShowFilter::All, &list_id()).args[1], "all");
    }

    #[test]
    fn info_argv() {
        assert_eq!(
            info(&reminder_id()).args,
            strings(&["info", "--json", "--no-input", "--", REMINDER])
        );
    }

    #[test]
    fn add_argv_minimal() {
        assert_eq!(
            add(&new_reminder()).args,
            strings(&[
                "add",
                "--json",
                "--no-input",
                "--title=Eggs",
                &format!("--list-id={WRITE_LIST}"),
            ])
        );
    }

    #[test]
    fn add_argv_full_with_dash_title() {
        let reminder = NewReminder {
            title: "-dash-test --list-id=other".to_owned(),
            notes: Some("--json\nline two".to_owned()),
            due: Some(Due::At(
                DateTime::parse_from_rfc3339("2026-10-06T09:00:00+02:00").unwrap(),
            )),
            repeat: Some(Repeat::Biweekly),
            priority: Some(Priority::High),
            location: Some(Trigger {
                place: shop(),
                proximity: Proximity::Leaving,
            }),
            ..new_reminder()
        };
        assert_eq!(
            add(&reminder).args,
            strings(&[
                "add",
                "--json",
                "--no-input",
                "--title=-dash-test --list-id=other",
                &format!("--list-id={WRITE_LIST}"),
                "--notes=--json\nline two",
                "--due=2026-10-06T09:00:00+02:00",
                "--repeat=biweekly",
                "--priority=high",
                &format!("--location={SHOP_ADDRESS}"),
                "--radius=150",
                "--leaving",
            ])
        );
    }

    #[test]
    fn add_argv_all_day_arriving() {
        let reminder = NewReminder {
            due: Some(Due::Day(NaiveDate::from_ymd_opt(2026, 10, 7).unwrap())),
            location: Some(Trigger {
                place: shop(),
                proximity: Proximity::Arriving,
            }),
            ..new_reminder()
        };
        assert_eq!(
            add(&reminder).args,
            strings(&[
                "add",
                "--json",
                "--no-input",
                "--title=Eggs",
                &format!("--list-id={WRITE_LIST}"),
                "--due=2026-10-07",
                &format!("--location={SHOP_ADDRESS}"),
                "--radius=150",
            ])
        );
    }

    #[test]
    fn edit_argv_full() {
        let changes = ReminderChanges {
            title: Some("-Renamed".to_owned()),
            notes: Some("Bring\tbag".to_owned()),
            due: Some(Change::Set(Due::Day(
                NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            ))),
            repeat: Some(Change::Set(Repeat::Yearly)),
            priority: Some(Priority::None),
            completion: Some(Completion::Complete),
        };
        assert_eq!(
            edit(&reminder_id(), &changes).args,
            strings(&[
                "edit",
                "--json",
                "--no-input",
                "--title=-Renamed",
                "--notes=Bring\tbag",
                "--due=2026-10-07",
                "--clear-alarm",
                "--repeat=yearly",
                "--priority=none",
                "--complete",
                "--",
                REMINDER,
            ])
        );
    }

    #[test]
    fn edit_argv_clears() {
        let changes = ReminderChanges {
            due: Some(Change::Clear),
            repeat: Some(Change::Clear),
            completion: Some(Completion::Incomplete),
            ..ReminderChanges::default()
        };
        assert_eq!(
            edit(&reminder_id(), &changes).args,
            strings(&[
                "edit",
                "--json",
                "--no-input",
                "--clear-due",
                "--clear-alarm",
                "--no-repeat",
                "--incomplete",
                "--",
                REMINDER,
            ])
        );
    }

    #[test]
    fn edit_argv_timed_due_moves_the_alarm() {
        let changes = ReminderChanges {
            due: Some(Change::Set(Due::At(
                DateTime::parse_from_rfc3339("2026-10-06T15:00:00+02:00").unwrap(),
            ))),
            ..ReminderChanges::default()
        };
        assert_eq!(
            edit(&reminder_id(), &changes).args,
            strings(&[
                "edit",
                "--json",
                "--no-input",
                "--due=2026-10-06T15:00:00+02:00",
                "--alarm=2026-10-06T15:00:00+02:00",
                "--",
                REMINDER,
            ])
        );
    }

    #[test]
    fn delete_argv() {
        assert_eq!(
            delete(&reminder_id()).args,
            strings(&["delete", "--json", "--no-input", "--force", "--", REMINDER])
        );
    }

    #[test]
    fn status_argv() {
        assert_eq!(status().args, strings(&["status", "--json", "--no-input"]));
    }

    #[test]
    fn index_like_ids_never_reach_argv() {
        for bad in ["1", "12", "1B2C", "1B2C3D4E-5F6A", "-1", "--force"] {
            assert!(ReminderId::parse(bad).is_err(), "{bad:?}");
            assert!(ListId::parse(bad.to_owned()).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn list_through_fake() {
        let fake = Fake::recording("remindctl_list.json");
        let runner = fake.remindctl_runner(StoreLock::default());
        let lists = runner.session().await.list().await.unwrap();
        assert_eq!(lists.len(), 3);
        assert_eq!(lists[0].id, list_id());
        assert_eq!(fake.recorded_args(), list().args);
    }

    #[tokio::test]
    async fn show_through_fake() {
        let fake = Fake::recording("remindctl_show.json");
        let runner = fake.remindctl_runner(StoreLock::default());
        let reminders = runner
            .session()
            .await
            .show(ShowFilter::All, &list_id())
            .await
            .unwrap();
        assert_eq!(reminders.len(), 5);
        assert_eq!(fake.recorded_args(), show(ShowFilter::All, &list_id()).args);
    }

    #[tokio::test]
    async fn info_through_fake() {
        let fake = Fake::recording("remindctl_info.json");
        let runner = fake.remindctl_runner(StoreLock::default());
        let reminder = runner.session().await.info(&reminder_id()).await.unwrap();
        assert_eq!(reminder.id, reminder_id());
        assert_eq!(reminder.list_id, list_id());
        assert_eq!(fake.recorded_args(), info(&reminder_id()).args);
    }

    #[tokio::test]
    async fn add_passes_argv_verbatim() {
        let fake = Fake::recording("remindctl_add.json");
        let runner = fake.remindctl_runner(StoreLock::default());
        let reminder = NewReminder {
            title: "-dash-test".to_owned(),
            notes: Some("multi\nline 'quoted' $HOME --json".to_owned()),
            ..new_reminder()
        };
        let created = runner.session().await.add(&reminder).await.unwrap();
        assert_eq!(created.title, "Eggs");
        assert_eq!(
            fake.recorded_args(),
            strings(&[
                "add",
                "--json",
                "--no-input",
                "--title=-dash-test",
                &format!("--list-id={WRITE_LIST}"),
                "--notes=multi\nline 'quoted' $HOME --json",
            ])
        );
    }

    #[tokio::test]
    async fn edit_through_fake() {
        let fake = Fake::recording("remindctl_edit.json");
        let runner = fake.remindctl_runner(StoreLock::default());
        let changes = ReminderChanges {
            completion: Some(Completion::Complete),
            ..ReminderChanges::default()
        };
        let edited = runner
            .session()
            .await
            .edit(&reminder_id(), &changes)
            .await
            .unwrap();
        assert!(edited.is_completed);
        assert_eq!(fake.recorded_args(), edit(&reminder_id(), &changes).args);
    }

    #[tokio::test]
    async fn delete_through_fake() {
        let fake = Fake::recording("remindctl_delete.json");
        let runner = fake.remindctl_runner(StoreLock::default());
        runner.session().await.delete(&reminder_id()).await.unwrap();
        assert_eq!(fake.recorded_args(), delete(&reminder_id()).args);
    }

    #[tokio::test]
    async fn status_through_fake() {
        let fake = Fake::recording("remindctl_status.json");
        let runner = fake.remindctl_runner(StoreLock::default());
        let reported = runner.session().await.status().await.unwrap();
        assert!(reported.authorized);
        assert_eq!(fake.recorded_args(), status().args);
    }

    #[tokio::test]
    async fn reminder_not_found() {
        let fake = Fake::new(&format!(
            "echo 'Reminder not found: \"{REMINDER}\".' >&2\nexit 1"
        ));
        let runner = fake.remindctl_runner(StoreLock::default());
        let err = runner
            .session()
            .await
            .info(&reminder_id())
            .await
            .unwrap_err();
        let RemindctlError::NotFound(message) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(message, &format!("Reminder not found: \"{REMINDER}\"."));
    }

    #[tokio::test]
    async fn reminder_not_found_from_a_write() {
        let fake = Fake::new("echo 'Reminder not found: \"x\".' >&2\nexit 1");
        let runner = fake.remindctl_runner(StoreLock::default());
        let err = runner
            .session()
            .await
            .delete(&reminder_id())
            .await
            .unwrap_err();
        let RemindctlError::NotFound(_) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn list_not_found() {
        let fake = Fake::new(&format!(
            "echo 'List not found: \"{WRITE_LIST}\".' >&2\nexit 1"
        ));
        let runner = fake.remindctl_runner(StoreLock::default());
        let err = runner
            .session()
            .await
            .show(ShowFilter::Open, &list_id())
            .await
            .unwrap_err();
        let RemindctlError::ListNotFound(message) = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(message, &format!("List not found: \"{WRITE_LIST}\"."));
        assert_eq!(
            err.to_string(),
            format!("remindctl: List not found: \"{WRITE_LIST}\".")
        );
    }

    #[tokio::test]
    async fn other_stderr_keeps_the_tail() {
        let fake = Fake::new(
            "i=0\nwhile [ $i -lt 100 ]; do printf 'noise-' >&2; i=$((i+1)); done\necho 'Reminders access denied' >&2\nexit 1",
        );
        let runner = fake.remindctl_runner(StoreLock::default());
        let err = runner.session().await.list().await.unwrap_err();
        let RemindctlError::Exit { code, reason } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(*code, Some(1));
        assert!(reason.len() <= STDERR_TAIL, "{}", reason.len());
        assert!(
            reason.ends_with("noise-Reminders access denied"),
            "{reason}"
        );
        assert!(
            err.to_string()
                .starts_with("remindctl exited with code 1: ")
        );
    }

    #[tokio::test]
    async fn failed_add_never_reports_the_place_address() {
        let fake = Fake::new(&format!(
            "echo 'Error: Could not geocode location: {SHOP_ADDRESS}' >&2\nexit 1"
        ));
        let runner = fake.remindctl_runner(StoreLock::default());
        let reminder = NewReminder {
            location: Some(Trigger {
                place: shop(),
                proximity: Proximity::Arriving,
            }),
            ..new_reminder()
        };
        let err = runner.session().await.add(&reminder).await.unwrap_err();
        let message = err.to_string();
        assert!(!message.contains(SHOP_ADDRESS), "{message}");
        assert_eq!(
            message,
            "remindctl exited with code 1: Error: Could not geocode location: place shop"
        );
    }

    #[tokio::test]
    async fn output_over_the_cap_is_killed() {
        let fake = Fake::new(&format!("exec head -c {} /dev/zero", STDOUT_CAP + 1));
        let runner = fake.remindctl_runner(StoreLock::default());
        let log = CallLog::default();
        let err = log
            .scope(async { runner.session().await.list().await.unwrap_err() })
            .await;
        let RemindctlError::OutputTooLarge = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(
            log.calls(),
            vec![Call {
                command: Command::List,
                outcome: CallOutcome::Killed,
            }]
        );
    }

    #[tokio::test]
    async fn not_found_text_on_success_is_ignored() {
        let fake = Fake::new(&format!(
            "echo 'Reminder not found' >&2\ncat '{}'",
            fixture("remindctl_info.json").display()
        ));
        let runner = fake.remindctl_runner(StoreLock::default());
        runner.session().await.info(&reminder_id()).await.unwrap();
    }

    #[tokio::test]
    async fn exit_without_stderr() {
        let fake = Fake::new("exit 2");
        let runner = fake.remindctl_runner(StoreLock::default());
        let err = runner.session().await.status().await.unwrap_err();
        assert_eq!(err.to_string(), "remindctl exited with code 2");
    }

    #[tokio::test]
    async fn killed_by_a_signal() {
        let fake = Fake::new("kill -9 $$");
        let runner = fake.remindctl_runner(StoreLock::default());
        let err = runner.session().await.status().await.unwrap_err();
        assert_eq!(err.to_string(), "remindctl was killed by a signal");
    }

    #[tokio::test]
    async fn output_that_is_not_json() {
        let fake = Fake::new("echo 'Reminders:'");
        let runner = fake.remindctl_runner(StoreLock::default());
        let err = runner.session().await.list().await.unwrap_err();
        let RemindctlError::UnexpectedOutput = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(err.to_string(), "unexpected remindctl output");
    }

    #[tokio::test]
    async fn json_of_the_wrong_shape() {
        let fake = Fake::printing("remindctl_status.json");
        let runner = fake.remindctl_runner(StoreLock::default());
        let err = runner
            .session()
            .await
            .info(&reminder_id())
            .await
            .unwrap_err();
        let RemindctlError::UnexpectedOutput = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn timeout_kills_the_child() {
        let fake = Fake::new("echo start >> \"$LOG\"\nsleep 1\necho end >> \"$LOG\"");
        let runner = Runner::new(
            fake.program().to_path_buf(),
            Duration::from_millis(300),
            StoreLock::default(),
        );
        let err = runner.session().await.list().await.unwrap_err();
        let RemindctlError::Timeout = err else {
            panic!("unexpected error: {err:?}");
        };
        time::sleep(Duration::from_millis(1500)).await;
        assert!(!fake.log().contains("end"), "{}", fake.log());
    }

    #[tokio::test]
    async fn missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(
            dir.path().join("remindctl"),
            Duration::from_secs(5),
            StoreLock::default(),
        );
        let err = runner.session().await.list().await.unwrap_err();
        let RemindctlError::Spawn(_) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn reminder_and_calendar_calls_never_overlap() {
        let fake = Fake::new(&format!(
            "echo \"$1 $2 start\" >> \"$LOG\"\nsleep 0.2\necho \"$1 $2 end\" >> \"$LOG\"\ncase \"$1 $2\" in\n  'list calendars') cat '{}' ;;\n  *) cat '{}' ;;\nesac",
            fixture("list_calendars.json").display(),
            fixture("remindctl_list.json").display()
        ));
        let calendars = Arc::new(fake.runner());
        let reminders = Arc::new(fake.remindctl_runner(calendars.lock()));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let calendars = Arc::clone(&calendars);
            tasks.push(tokio::spawn(async move {
                calendars.session().await.list_calendars().await.unwrap();
            }));
            let reminders = Arc::clone(&reminders);
            tasks.push(tokio::spawn(async move {
                reminders.session().await.list().await.unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let log = fake.log();
        let mut lines = Vec::new();
        for line in log.lines() {
            lines.push(line);
        }
        assert_eq!(lines.len(), 8, "{log}");
        let mut reminder_calls = 0;
        for pair in lines.chunks(2) {
            let start = pair[0].strip_suffix(" start").unwrap();
            let end = pair[1].strip_suffix(" end").unwrap();
            assert_eq!(start, end, "{log}");
            if start == "list --json" {
                reminder_calls += 1;
            }
        }
        assert_eq!(reminder_calls, 2, "{log}");
    }

    #[tokio::test]
    async fn session_holds_the_shared_lock_across_calls() {
        let fake = Fake::new(&format!(
            "echo \"$1 start\" >> \"$LOG\"\nsleep 0.1\necho \"$1 end\" >> \"$LOG\"\ncase \"$1\" in\n  info) cat '{}' ;;\n  delete) cat '{}' ;;\n  *) cat '{}' ;;\nesac",
            fixture("remindctl_info.json").display(),
            fixture("remindctl_delete.json").display(),
            fixture("list_calendars.json").display()
        ));
        let calendars = Arc::new(fake.runner());
        let reminders = fake.remindctl_runner(calendars.lock());
        let session = reminders.session().await;
        let other = {
            let calendars = Arc::clone(&calendars);
            tokio::spawn(async move { calendars.session().await.list_calendars().await })
        };
        session.info(&reminder_id()).await.unwrap();
        session.delete(&reminder_id()).await.unwrap();
        drop(session);
        other.await.unwrap().unwrap();
        assert_eq!(
            fake.log(),
            "info start\ninfo end\ndelete start\ndelete end\nlist start\nlist end\n"
        );
    }

    #[tokio::test]
    async fn call_log_records_calls_made_in_scope() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Runner::new(
            dir.path().join("remindctl"),
            Duration::from_secs(5),
            StoreLock::default(),
        );
        let not_found = Fake::new("echo 'Reminder not found: \"x\".' >&2\nexit 1");
        let not_found = not_found.remindctl_runner(StoreLock::default());
        let ok = Fake::printing("remindctl_status.json");
        let ok = ok.remindctl_runner(StoreLock::default());
        let log = CallLog::default();
        log.scope(async {
            ok.session().await.status().await.unwrap();
            not_found
                .session()
                .await
                .info(&reminder_id())
                .await
                .unwrap_err();
            missing.session().await.list().await.unwrap_err();
            ok.session().await.list().await.unwrap_err();
        })
        .await;
        ok.session().await.status().await.unwrap();
        let calls = log.calls();
        assert_eq!(
            calls[0],
            Call {
                command: Command::Status,
                outcome: CallOutcome::Exited(0),
            }
        );
        let mut rendered = Vec::new();
        for call in calls {
            rendered.push(call.to_string());
        }
        assert_eq!(
            rendered,
            vec!["status=0", "info=1", "list=not started", "list=0"]
        );
    }

    #[tokio::test]
    async fn call_log_outcomes_for_killed_children() {
        let signalled = Fake::new("kill -9 $$");
        let signalled = signalled.remindctl_runner(StoreLock::default());
        let slow = Fake::new("sleep 2");
        let slow = Runner::new(
            slow.program().to_path_buf(),
            Duration::from_millis(100),
            StoreLock::default(),
        );
        let log = CallLog::default();
        log.scope(async {
            signalled.session().await.status().await.unwrap_err();
            slow.session().await.status().await.unwrap_err();
        })
        .await;
        let mut rendered = Vec::new();
        for call in log.calls() {
            rendered.push(call.to_string());
        }
        assert_eq!(rendered, vec!["status=signalled", "status=timeout"]);
    }
}
