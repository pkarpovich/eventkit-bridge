use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::config::Config;
use crate::ekctl::{EkctlError, Runner};
use crate::policy::Policy;

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
}

/// The result of a health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Every readable calendar exists.
    Ok {
        /// How many readable calendars exist.
        calendars: usize,
    },
    /// The bridge cannot serve reads as configured.
    Degraded(DegradedReason),
}

impl IntoResponse for Health {
    fn into_response(self) -> Response {
        match self {
            Health::Ok { calendars } => (
                StatusCode::OK,
                Json(json!({
                    "status": "ok",
                    "version": env!("CARGO_PKG_VERSION"),
                    "calendars": calendars,
                })),
            )
                .into_response(),
            Health::Degraded(reason) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status": "degraded", "reason": reason})),
            )
                .into_response(),
        }
    }
}

/// Runs `ekctl list calendars` at most once per TTL; concurrent callers wait for one check.
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
    pub async fn check(&self, runner: &Runner, policy: &Policy) -> Health {
        if !self.configured {
            return Health::Degraded(DegradedReason::Unconfigured);
        }
        let mut cache = self.cache.lock().await;
        if let Some((checked_at, health)) = *cache
            && checked_at.elapsed() < self.ttl
        {
            return health;
        }
        let health = probe(runner, policy).await;
        *cache = Some((Instant::now(), health));
        health
    }
}

async fn probe(runner: &Runner, policy: &Policy) -> Health {
    let calendars = match runner.session().await.list_calendars().await {
        Ok(calendars) => calendars,
        Err(err) => return Health::Degraded(failure_reason(&err)),
    };
    let existing = policy.filter_calendars(calendars).len();
    if existing < policy.default_read_set().len() {
        return Health::Degraded(DegradedReason::CalendarMissing);
    }
    Health::Ok {
        calendars: existing,
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
    use std::sync::Arc;

    use super::*;
    use crate::fake_ekctl::{Fake, fixture};

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const MISSING_ID: &str = "11111111-2222-3333-4444-555555555555";

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

    fn counting(delay: &str) -> Fake {
        Fake::new(&format!(
            "echo call >> \"$LOG\"\nsleep {delay}\ncat '{}'",
            fixture("list_calendars.json").display()
        ))
    }

    async fn check(config: &Config, fake: &Fake) -> Health {
        HealthCheck::new(config, HEALTH_TTL)
            .check(&fake.runner(), &Policy::new(config))
            .await
    }

    #[tokio::test]
    async fn ok_counts_readable_calendars() {
        let config = config(&[READ_ID], Some(WRITE_ID));
        let fake = Fake::printing("list_calendars.json");
        assert_eq!(check(&config, &fake).await, Health::Ok { calendars: 2 });
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
        let health = HealthCheck::new(&config, HEALTH_TTL)
            .check(
                &fake.runner_with_timeout(Duration::from_millis(200)),
                &Policy::new(&config),
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
        let health = HealthCheck::new(&config, HEALTH_TTL)
            .check(&runner, &Policy::new(&config))
            .await;
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
    async fn result_is_cached() {
        let config = config(&[READ_ID], None);
        let fake = counting("0");
        let runner = fake.runner();
        let policy = Policy::new(&config);
        let health = HealthCheck::new(&config, HEALTH_TTL);
        for _ in 0..3 {
            assert_eq!(
                health.check(&runner, &policy).await,
                Health::Ok { calendars: 1 }
            );
        }
        assert_eq!(fake.log(), "call\n");
    }

    #[tokio::test]
    async fn expired_result_is_refreshed() {
        let config = config(&[READ_ID], None);
        let fake = counting("0");
        let runner = fake.runner();
        let policy = Policy::new(&config);
        let health = HealthCheck::new(&config, Duration::from_millis(50));
        health.check(&runner, &policy).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        health.check(&runner, &policy).await;
        assert_eq!(fake.log(), "call\ncall\n");
    }

    #[tokio::test]
    async fn concurrent_checks_share_one_run() {
        let config = config(&[READ_ID], None);
        let fake = counting("0.3");
        let runner = Arc::new(fake.runner());
        let policy = Arc::new(Policy::new(&config));
        let health = Arc::new(HealthCheck::new(&config, HEALTH_TTL));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let runner = Arc::clone(&runner);
            let policy = Arc::clone(&policy);
            let health = Arc::clone(&health);
            tasks.push(tokio::spawn(
                async move { health.check(&runner, &policy).await },
            ));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), Health::Ok { calendars: 1 });
        }
        assert_eq!(fake.log(), "call\n");
    }

    #[test]
    fn response_bodies() {
        let response = Health::Degraded(DegradedReason::EkctlFailed).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            serde_json::to_value(DegradedReason::CalendarMissing).unwrap(),
            json!("calendar missing")
        );
        assert_eq!(
            Health::Ok { calendars: 1 }.into_response().status(),
            StatusCode::OK
        );
    }
}
