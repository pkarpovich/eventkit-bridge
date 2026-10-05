use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Serialize;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::config::Config;
use crate::ekctl::{self, EkctlError};
use crate::mail::store::{MailProblem, MailStatus, MailStore};
use crate::policy::Policy;
use crate::remindctl::{self, RemindctlError};
use crate::reminders_model::RcStatus;

/// How long one health result is reused.
pub const HEALTH_TTL: Duration = Duration::from_secs(10);

/// Why the bridge reports itself degraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DegradedReason {
    /// `read_calendars` is empty.
    #[serde(rename = "unconfigured")]
    Unconfigured,
    /// `ekctl list calendars` ran past its deadline.
    #[serde(rename = "timeout")]
    Timeout,
    /// `ekctl list calendars` failed, as it does without the calendar permission.
    #[serde(rename = "ekctl failed")]
    EkctlFailed,
    /// A configured calendar id no longer exists.
    #[serde(rename = "calendar missing")]
    CalendarMissing,
    /// `remindctl status` reports that the app may not use reminders.
    #[serde(rename = "reminders access missing")]
    RemindersAccessMissing,
    /// `remindctl` failed for another reason.
    #[serde(rename = "remindctl failed")]
    RemindctlFailed,
    /// The Envelope Index cannot be opened or read, as without Full Disk Access.
    #[serde(rename = "mail no access")]
    MailNoAccess,
    /// A table or column the bridge reads from the Envelope Index is missing.
    #[serde(rename = "mail schema changed")]
    MailSchemaChanged,
    /// A configured mail account has no mailboxes.
    #[serde(rename = "mail account missing")]
    MailAccountMissing,
}

/// The result of a health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Every readable calendar exists.
    Ok {
        /// How many readable calendars exist.
        calendars: usize,
        /// How many readable reminder lists exist, `None` when none are configured.
        lists: Option<usize>,
        /// The mail store, `None` when `[mail]` is absent.
        mail: Option<MailStatus>,
    },
    /// The bridge cannot serve reads as configured.
    Degraded(DegradedReason),
}

impl IntoResponse for Health {
    fn into_response(self) -> Response {
        match self {
            Health::Ok {
                calendars,
                lists,
                mail,
            } => {
                let mut body = json!({
                    "status": "ok",
                    "version": env!("CARGO_PKG_VERSION"),
                    "calendars": calendars,
                });
                if let Some(lists) = lists {
                    body["lists"] = json!(lists);
                }
                if let Some(MailStatus {
                    accounts,
                    newest_message_age_s,
                }) = mail
                {
                    body["mail_accounts"] = json!(accounts);
                    body["newest_message_age_s"] = json!(newest_message_age_s);
                }
                (StatusCode::OK, Json(body)).into_response()
            }
            Health::Degraded(reason) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status": "degraded", "reason": reason})),
            )
                .into_response(),
        }
    }
}

/// What a health check runs and checks against.
#[derive(Debug, Clone, Copy)]
pub struct Probe<'a> {
    /// The `ekctl` runner.
    pub calendars: &'a ekctl::Runner,
    /// The `remindctl` runner, used only when reminder lists are configured.
    pub reminders: &'a remindctl::Runner,
    /// The policy naming the readable calendars and lists.
    pub policy: &'a Policy,
    /// The mail store, `None` when `[mail]` is absent.
    pub mail: Option<&'a Arc<MailStore>>,
}

/// Runs `ekctl list calendars`, `remindctl status` and `list` when reminder lists are
/// configured, and the mail store check when `[mail]` is present, at most once per TTL;
/// concurrent callers wait for one check.
#[derive(Debug)]
pub struct HealthCheck {
    configured: bool,
    ttl: Duration,
    cache: Mutex<Option<(Instant, Health)>>,
}

impl HealthCheck {
    /// A check for `config` that reuses each result for `ttl`.
    pub fn new(config: &Config, ttl: Duration) -> Self {
        Self {
            configured: !config.read_calendars.is_empty(),
            ttl,
            cache: Mutex::new(None),
        }
    }

    /// The current health, from the cache when it is fresh.
    pub async fn check(&self, probe: Probe<'_>) -> Health {
        if !self.configured {
            return Health::Degraded(DegradedReason::Unconfigured);
        }
        let mut cache = self.cache.lock().await;
        if let Some((checked_at, health)) = *cache
            && checked_at.elapsed() < self.ttl
        {
            return health;
        }
        let health = run_probe(probe).await;
        *cache = Some((Instant::now(), health));
        health
    }
}

async fn run_probe(probe: Probe<'_>) -> Health {
    match probe_all(probe).await {
        Ok(health) => health,
        Err(reason) => Health::Degraded(reason),
    }
}

async fn probe_all(probe: Probe<'_>) -> Result<Health, DegradedReason> {
    let Probe {
        calendars,
        reminders,
        policy,
        mail,
    } = probe;
    let listed = match calendars.session().await.list_calendars().await {
        Ok(listed) => listed,
        Err(err) => return Err(failure_reason(&err)),
    };
    let existing = policy.filter_calendars(listed).len();
    if existing < policy.default_read_set().len() {
        return Err(DegradedReason::CalendarMissing);
    }
    let lists = match policy.any_readable_list() {
        true => Some(probe_lists(reminders, policy).await?),
        false => None,
    };
    let mail = match mail {
        Some(store) => Some(probe_mail(store).await?),
        None => None,
    };
    Ok(Health::Ok {
        calendars: existing,
        lists,
        mail,
    })
}

async fn probe_mail(store: &Arc<MailStore>) -> Result<MailStatus, DegradedReason> {
    let store = Arc::clone(store);
    let status = tokio::task::spawn_blocking(move || store.status(Utc::now())).await;
    match status {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(problem)) => Err(mail_reason(problem)),
        Err(err) => {
            tracing::warn!(error = %err, "the mail check did not finish");
            Err(DegradedReason::MailNoAccess)
        }
    }
}

fn mail_reason(problem: MailProblem) -> DegradedReason {
    match problem {
        MailProblem::NoAccess => DegradedReason::MailNoAccess,
        MailProblem::SchemaChanged => DegradedReason::MailSchemaChanged,
        MailProblem::AccountMissing => DegradedReason::MailAccountMissing,
    }
}

async fn probe_lists(runner: &remindctl::Runner, policy: &Policy) -> Result<usize, DegradedReason> {
    let session = runner.session().await;
    let RcStatus {
        authorized,
        status: _,
    } = session
        .status()
        .await
        .map_err(|err| remindctl_reason(&err))?;
    if !authorized {
        return Err(DegradedReason::RemindersAccessMissing);
    }
    let lists = session.list().await.map_err(|err| remindctl_reason(&err))?;
    Ok(policy.filter_lists(lists).len())
}

fn remindctl_reason(err: &RemindctlError) -> DegradedReason {
    match err {
        RemindctlError::Timeout => DegradedReason::Timeout,
        RemindctlError::Spawn(_)
        | RemindctlError::Io(_)
        | RemindctlError::OutputTooLarge
        | RemindctlError::NotFound(_)
        | RemindctlError::ListNotFound(_)
        | RemindctlError::Exit { code: _, reason: _ }
        | RemindctlError::UnexpectedOutput => DegradedReason::RemindctlFailed,
    }
}

fn failure_reason(err: &EkctlError) -> DegradedReason {
    match err {
        EkctlError::Timeout => DegradedReason::Timeout,
        EkctlError::Spawn(_)
        | EkctlError::Io(_)
        | EkctlError::OutputTooLarge
        | EkctlError::Exit { code: _, reason: _ }
        | EkctlError::NotFound(_)
        | EkctlError::Reported(_)
        | EkctlError::UnexpectedOutput => DegradedReason::EkctlFailed,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::ekctl::Runner;
    use crate::fake_ekctl::{Fake, fixture};
    use crate::subprocess::StoreLock;

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const MISSING_ID: &str = "11111111-2222-3333-4444-555555555555";
    const ABSENT_LIST: &str = "99999999-2222-3333-4444-555555555555";

    fn config(read: &[&str], write: Option<&str>) -> Config {
        let mut toml = String::from("listen = \"127.0.0.1:8790\"\nread_calendars = [");
        for id in read {
            toml.push_str(&format!("\"{id}\","));
        }
        toml.push_str("]\n");
        if let Some(write) = write {
            toml.push_str(&format!("write_calendars = [\"{write}\"]\n"));
        }
        Config::from_toml(&toml).unwrap()
    }

    fn with_lists(read_lists: &[&str]) -> Config {
        let mut toml = format!(
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\"]\nwrite_lists = [\"{WRITE_ID}\"]\nread_lists = ["
        );
        for id in read_lists {
            toml.push_str(&format!("\"{id}\","));
        }
        toml.push_str("]\n");
        Config::from_toml(&toml).unwrap()
    }

    fn counting(delay: &str) -> Fake {
        Fake::new(&format!(
            "echo call >> \"$LOG\"\nsleep {delay}\ncat '{}'",
            fixture("list_calendars.json").display()
        ))
    }

    fn reminders_fake(status: &str) -> Fake {
        Fake::new(&format!(
            "echo \"$1\" >> \"$LOG\"\ncase \"$1\" in\n  status) echo '{status}' ;;\n  list) if [ \"$2\" = calendars ]; then cat '{}'; else cat '{}'; fi ;;\nesac",
            fixture("list_calendars.json").display(),
            fixture("remindctl_list.json").display()
        ))
    }

    fn absent_remindctl(calendars: &Runner) -> remindctl::Runner {
        remindctl::Runner::new(
            Path::new("/nonexistent/remindctl").to_path_buf(),
            Duration::from_secs(5),
            calendars.lock(),
        )
    }

    async fn check_with(config: &Config, calendars: &Runner) -> Health {
        let reminders = absent_remindctl(calendars);
        HealthCheck::new(config, HEALTH_TTL)
            .check(Probe {
                calendars,
                reminders: &reminders,
                policy: &Policy::new(config),
                mail: None,
            })
            .await
    }

    async fn check(config: &Config, fake: &Fake) -> Health {
        check_with(config, &fake.runner()).await
    }

    async fn check_reminders(config: &Config, fake: &Fake) -> Health {
        let calendars = fake.runner();
        let reminders = fake.remindctl_runner(calendars.lock());
        HealthCheck::new(config, HEALTH_TTL)
            .check(Probe {
                calendars: &calendars,
                reminders: &reminders,
                policy: &Policy::new(config),
                mail: None,
            })
            .await
    }

    #[tokio::test]
    async fn ok_counts_readable_calendars() {
        let config = config(&[READ_ID], Some(WRITE_ID));
        let fake = Fake::printing("list_calendars.json");
        assert_eq!(
            check(&config, &fake).await,
            Health::Ok {
                calendars: 2,
                lists: None,
                mail: None
            }
        );
    }

    #[tokio::test]
    async fn unconfigured_without_running_ekctl() {
        let config = config(&[], Some(WRITE_ID));
        let fake = counting("0");
        assert_eq!(
            check(&config, &fake).await,
            Health::Degraded(DegradedReason::Unconfigured)
        );
        assert_eq!(fake.log(), "");
    }

    #[tokio::test]
    async fn timeout() {
        let config = config(&[READ_ID], None);
        let fake = counting("2");
        let health = check_with(
            &config,
            &fake.runner_with_timeout(Duration::from_millis(200)),
        )
        .await;
        assert_eq!(health, Health::Degraded(DegradedReason::Timeout));
    }

    #[tokio::test]
    async fn ekctl_failed() {
        let config = config(&[READ_ID], None);
        for body in [
            "echo 'not authorized' >&2\nexit 1",
            r#"echo '{"status":"error","error":"Calendar access denied"}'"#,
        ] {
            let fake = Fake::new(body);
            assert_eq!(
                check(&config, &fake).await,
                Health::Degraded(DegradedReason::EkctlFailed)
            );
        }
    }

    #[tokio::test]
    async fn calendar_missing() {
        let config = config(&[READ_ID, MISSING_ID], None);
        let fake = Fake::printing("list_calendars.json");
        assert_eq!(
            check(&config, &fake).await,
            Health::Degraded(DegradedReason::CalendarMissing)
        );
    }

    #[tokio::test]
    async fn missing_write_calendar_is_missing() {
        let config = config(&[READ_ID], Some(MISSING_ID));
        let fake = Fake::printing("list_calendars.json");
        assert_eq!(
            check(&config, &fake).await,
            Health::Degraded(DegradedReason::CalendarMissing)
        );
    }

    #[tokio::test]
    async fn missing_ekctl_binary_is_ekctl_failed() {
        let config = config(&[READ_ID], None);
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(dir.path().join("ekctl"), Duration::from_secs(5));
        let health = check_with(&config, &runner).await;
        assert_eq!(health, Health::Degraded(DegradedReason::EkctlFailed));
    }

    #[tokio::test]
    async fn reminder_list_counts_as_missing() {
        let config = config(&[READ_ID, "2F8BCC68-AD77-B8A4-9218-37BF6271D47D"], None);
        let fake = Fake::printing("list_calendars.json");
        assert_eq!(
            check(&config, &fake).await,
            Health::Degraded(DegradedReason::CalendarMissing)
        );
    }

    #[tokio::test]
    async fn no_lists_configured_never_runs_remindctl() {
        let config = config(&[READ_ID], None);
        let fake = reminders_fake(r#"{"authorized":false,"status":"denied"}"#);
        assert_eq!(
            check_reminders(&config, &fake).await,
            Health::Ok {
                calendars: 1,
                lists: None,
                mail: None
            }
        );
        assert_eq!(fake.log(), "list\n");
    }

    #[tokio::test]
    async fn ok_counts_readable_lists() {
        let config = with_lists(&[READ_ID, ABSENT_LIST]);
        let fake = reminders_fake(r#"{"authorized":true,"status":"full-access"}"#);
        assert_eq!(
            check_reminders(&config, &fake).await,
            Health::Ok {
                calendars: 1,
                lists: Some(2),
                mail: None
            }
        );
        assert_eq!(fake.log(), "list\nstatus\nlist\n");
    }

    #[tokio::test]
    async fn reminders_access_missing() {
        let config = with_lists(&[]);
        let fake = reminders_fake(r#"{"authorized":false,"status":"denied"}"#);
        assert_eq!(
            check_reminders(&config, &fake).await,
            Health::Degraded(DegradedReason::RemindersAccessMissing)
        );
        assert_eq!(fake.log(), "list\nstatus\n");
    }

    #[tokio::test]
    async fn remindctl_failures() {
        let config = with_lists(&[]);
        let calendars = Fake::printing("list_calendars.json");
        let calendars = calendars.runner();
        let failing = Fake::new("echo 'boom' >&2\nexit 1");
        let reminders = failing.remindctl_runner(StoreLock::default());
        let health = HealthCheck::new(&config, HEALTH_TTL)
            .check(Probe {
                calendars: &calendars,
                reminders: &reminders,
                policy: &Policy::new(&config),
                mail: None,
            })
            .await;
        assert_eq!(health, Health::Degraded(DegradedReason::RemindctlFailed));
        let slow = Fake::new("sleep 2");
        let reminders = remindctl::Runner::new(
            slow.program().to_path_buf(),
            Duration::from_millis(200),
            StoreLock::default(),
        );
        let health = HealthCheck::new(&config, HEALTH_TTL)
            .check(Probe {
                calendars: &calendars,
                reminders: &reminders,
                policy: &Policy::new(&config),
                mail: None,
            })
            .await;
        assert_eq!(health, Health::Degraded(DegradedReason::Timeout));
    }

    #[tokio::test]
    async fn result_is_cached() {
        let config = config(&[READ_ID], None);
        let fake = counting("0");
        let runner = fake.runner();
        let reminders = absent_remindctl(&runner);
        let policy = Policy::new(&config);
        let health = HealthCheck::new(&config, HEALTH_TTL);
        for _ in 0..3 {
            let probe = Probe {
                calendars: &runner,
                reminders: &reminders,
                policy: &policy,
                mail: None,
            };
            assert_eq!(
                health.check(probe).await,
                Health::Ok {
                    calendars: 1,
                    lists: None,
                    mail: None
                }
            );
        }
        assert_eq!(fake.log(), "call\n");
    }

    #[tokio::test]
    async fn expired_result_is_refreshed() {
        let config = config(&[READ_ID], None);
        let fake = counting("0");
        let runner = fake.runner();
        let reminders = absent_remindctl(&runner);
        let policy = Policy::new(&config);
        let probe = Probe {
            calendars: &runner,
            reminders: &reminders,
            policy: &policy,
            mail: None,
        };
        let health = HealthCheck::new(&config, Duration::from_millis(50));
        health.check(probe).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        health.check(probe).await;
        assert_eq!(fake.log(), "call\ncall\n");
    }

    #[tokio::test]
    async fn concurrent_checks_share_one_run() {
        let config = config(&[READ_ID], None);
        let fake = counting("0.3");
        let runner = Arc::new(fake.runner());
        let reminders = Arc::new(absent_remindctl(&runner));
        let policy = Arc::new(Policy::new(&config));
        let health = Arc::new(HealthCheck::new(&config, HEALTH_TTL));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let runner = Arc::clone(&runner);
            let reminders = Arc::clone(&reminders);
            let policy = Arc::clone(&policy);
            let health = Arc::clone(&health);
            tasks.push(tokio::spawn(async move {
                health
                    .check(Probe {
                        calendars: &runner,
                        reminders: &reminders,
                        policy: &policy,
                        mail: None,
                    })
                    .await
            }));
        }
        for task in tasks {
            assert_eq!(
                task.await.unwrap(),
                Health::Ok {
                    calendars: 1,
                    lists: None,
                    mail: None
                }
            );
        }
        assert_eq!(fake.log(), "call\n");
    }

    async fn body(health: Health) -> serde_json::Value {
        let response = health.into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn response_bodies() {
        let response = Health::Degraded(DegradedReason::EkctlFailed).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            serde_json::to_value(DegradedReason::CalendarMissing).unwrap(),
            json!("calendar missing")
        );
        assert_eq!(
            body(Health::Degraded(DegradedReason::RemindersAccessMissing)).await,
            json!({"status": "degraded", "reason": "reminders access missing"})
        );
        let ok = Health::Ok {
            calendars: 1,
            lists: None,
            mail: None,
        };
        assert_eq!(ok.into_response().status(), StatusCode::OK);
        assert_eq!(
            body(ok).await,
            json!({"status": "ok", "version": env!("CARGO_PKG_VERSION"), "calendars": 1})
        );
        assert_eq!(
            body(Health::Ok {
                calendars: 1,
                lists: Some(3),
                mail: None
            })
            .await,
            json!({"status": "ok", "version": env!("CARGO_PKG_VERSION"), "calendars": 1, "lists": 3})
        );
    }
}
