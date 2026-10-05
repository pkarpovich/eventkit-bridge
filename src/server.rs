use std::future::{Future, IntoFuture};
use std::io;
use std::mem;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection};
use axum::extract::{DefaultBodyLimit, MatchedPath, Path, RawQuery, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::{self, Instant};

use crate::config::Config;
use crate::ekctl::{CallLog, EkctlError, EventRange, Runner};
use crate::health::{HEALTH_TTL, HealthCheck};
use crate::model::{CalendarKind, EventId, InvalidEventId};
use crate::policy::{GuardError, Policy, PolicyError};
use crate::request::{self, EventsRequest, Invalid};

/// The largest request body the bridge accepts.
pub const BODY_LIMIT: usize = 64 * 1024;

/// How long in-flight requests may run after shutdown starts.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(25);

/// The state every route shares.
#[derive(Debug)]
pub struct App {
    runner: Runner,
    policy: Policy,
    health: HealthCheck,
}

impl App {
    /// The bridge for `config`, running `ekctl` through `runner`.
    pub fn new(config: &Config, runner: Runner) -> Self {
        Self {
            runner,
            policy: Policy::new(config),
            health: HealthCheck::new(config, HEALTH_TTL),
        }
    }

    /// Logs every event calendar with its access once `ekctl list calendars` succeeds,
    /// retrying every `retry` until it does.
    pub async fn announce_calendars(&self, retry: Duration) {
        loop {
            match self.runner.session().await.list_calendars().await {
                Ok(calendars) => {
                    for calendar in calendars {
                        match calendar.kind {
                            CalendarKind::Event => {}
                            CalendarKind::Reminder | CalendarKind::Other => continue,
                        }
                        tracing::info!(
                            id = %calendar.id,
                            title = calendar.title.as_deref().unwrap_or(""),
                            source = calendar.source.as_deref().unwrap_or(""),
                            readable = self.policy.readable(&calendar.id),
                            writable = self.policy.writable(&calendar.id),
                            "calendar"
                        );
                    }
                    return;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "cannot list calendars yet, retrying");
                    time::sleep(retry).await;
                }
            }
        }
    }
}

/// An error answered as `{"error": message}`.
#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    Policy(PolicyError),
    Ekctl(EkctlError),
    Status(StatusCode, String),
}

impl From<Invalid> for ApiError {
    fn from(err: Invalid) -> Self {
        ApiError::BadRequest(err.to_string())
    }
}

impl From<PolicyError> for ApiError {
    fn from(err: PolicyError) -> Self {
        ApiError::Policy(err)
    }
}

impl From<EkctlError> for ApiError {
    fn from(err: EkctlError) -> Self {
        ApiError::Ekctl(err)
    }
}

impl From<GuardError> for ApiError {
    fn from(err: GuardError) -> Self {
        match err {
            GuardError::Denied(err) => ApiError::Policy(err),
            GuardError::Ekctl(err) => ApiError::Ekctl(err),
        }
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        ApiError::Status(rejection.status(), rejection.body_text())
    }
}

impl From<BytesRejection> for ApiError {
    fn from(rejection: BytesRejection) -> Self {
        ApiError::Status(rejection.status(), rejection.body_text())
    }
}

fn ekctl_status(err: &EkctlError) -> StatusCode {
    match err {
        EkctlError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        EkctlError::NotFound(_) => StatusCode::NOT_FOUND,
        EkctlError::Spawn(_)
        | EkctlError::Io(_)
        | EkctlError::OutputTooLarge
        | EkctlError::Exit { code: _, stderr: _ }
        | EkctlError::Reported(_)
        | EkctlError::UnexpectedOutput => StatusCode::BAD_GATEWAY,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            ApiError::Policy(err) => {
                tracing::warn!(reason = %err, "policy refusal");
                (StatusCode::FORBIDDEN, err.to_string())
            }
            ApiError::Ekctl(err) => (ekctl_status(&err), err.to_string()),
            ApiError::Status(status, message) => (status, message),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

type Shared = State<Arc<App>>;

/// The bridge's HTTP routes, with the body limit and request logging.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/calendars", get(list_calendars))
        .route("/v1/events", get(list_events).post(create_event))
        .route(
            "/v1/events/",
            get(empty_event_id)
                .patch(empty_event_id)
                .delete(empty_event_id),
        )
        .route(
            "/v1/events/{id}",
            get(show_event).patch(update_event).delete(delete_event),
        )
        .route("/v1/free", get(free))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn(log_request))
        .with_state(app)
}

/// Serves `app` on `listener` until `shutdown` resolves, then lets in-flight requests finish
/// for at most `grace`.
pub async fn serve(
    listener: TcpListener,
    app: Arc<App>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    grace: Duration,
) -> io::Result<()> {
    let (stopping, mut stopped) = watch::channel(false);
    let server = axum::serve(listener, router(app))
        .with_graceful_shutdown(async move {
            shutdown.await;
            stopping.send_replace(true);
        })
        .into_future();
    let deadline = async move {
        if stopped.wait_for(|stopping| *stopping).await.is_err() {
            std::future::pending::<()>().await;
        }
        time::sleep(grace).await;
    };
    tokio::select! {
        result = server => result,
        () = deadline => {
            tracing::warn!(grace_seconds = grace.as_secs(), "requests still running after the shutdown grace period");
            Ok(())
        }
    }
}

async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let route = match request.extensions().get::<MatchedPath>() {
        Some(path) => path.as_str().to_owned(),
        None => "unmatched".to_owned(),
    };
    let started = Instant::now();
    let calls = CallLog::default();
    let response = calls.scope(next.run(request)).await;
    let status = response.status().as_u16();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let calls = calls.calls();
    if calls.is_empty() {
        tracing::info!(%method, %route, status, duration_ms, "request");
        return response;
    }
    let mut ekctl = String::new();
    for call in calls {
        if !ekctl.is_empty() {
            ekctl.push_str(", ");
        }
        ekctl.push_str(&call.to_string());
    }
    tracing::info!(%method, %route, status, duration_ms, ekctl, "request");
    response
}

fn event_id(id: &str) -> Result<EventId, ApiError> {
    match EventId::parse(id) {
        Ok(id) => Ok(id),
        Err(err) => Err(ApiError::BadRequest(err.to_string())),
    }
}

async fn healthz(State(app): Shared) -> Response {
    app.health
        .check(&app.runner, &app.policy)
        .await
        .into_response()
}

async fn list_calendars(State(app): Shared) -> Result<Response, ApiError> {
    let calendars = app.runner.session().await.list_calendars().await?;
    let calendars = app.policy.filter_calendars(calendars);
    Ok(Json(json!({ "calendars": calendars })).into_response())
}

async fn list_events(State(app): Shared, RawQuery(query): RawQuery) -> Result<Response, ApiError> {
    let EventsRequest {
        calendars,
        from,
        to,
    } = request::events_query(query.as_deref())?;
    let calendars = app.policy.require_readable(calendars)?;
    let range = EventRange {
        calendars,
        from,
        to,
    };
    let events = app.runner.session().await.list_events(&range).await?;
    let mut readable = Vec::new();
    for event in events {
        if app.policy.readable(&event.calendar.id) {
            readable.push(event);
        }
    }
    Ok(Json(json!({ "events": readable })).into_response())
}

async fn show_event(
    State(app): Shared,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = event_id(&id)?;
    let event = app.runner.session().await.show_event(&id).await?;
    app.policy.require_event_readable(&event)?;
    Ok(Json(event).into_response())
}

async fn free(State(app): Shared, RawQuery(query): RawQuery) -> Result<Response, ApiError> {
    let mut query = request::free_query(query.as_deref())?;
    query.calendars = app
        .policy
        .require_readable(mem::take(&mut query.calendars))?;
    let slots = app.runner.session().await.free(&query).await?;
    Ok(Json(slots).into_response())
}

async fn create_event(
    State(app): Shared,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let event = request::create_body(&body?)?;
    let calendar = app.policy.write_calendar()?;
    let session = app.runner.session().await;
    let id = session.add_event(calendar, &event).await?;
    let created = session.show_event(&id).await?;
    Ok((StatusCode::CREATED, Json(created)).into_response())
}

async fn update_event(
    State(app): Shared,
    id: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = event_id(&id)?;
    let changes = request::update_body(&body?)?;
    let session = app.runner.session().await;
    let existing = app.policy.guard_write(&session, &id).await?;
    request::merged_range(&changes, &existing)?;
    let id = session.update_event(&id, &changes).await?;
    let updated = session.show_event(&id).await?;
    Ok(Json(updated).into_response())
}

async fn delete_event(
    State(app): Shared,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = event_id(&id)?;
    let session = app.runner.session().await;
    app.policy.guard_write(&session, &id).await?;
    session.delete_event(&id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn empty_event_id() -> ApiError {
    ApiError::BadRequest(InvalidEventId::Empty.to_string())
}

async fn not_found() -> ApiError {
    ApiError::Status(StatusCode::NOT_FOUND, "not found".to_owned())
}

async fn method_not_allowed() -> ApiError {
    ApiError::Status(
        StatusCode::METHOD_NOT_ALLOWED,
        "method not allowed".to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::http::Method;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tower::ServiceExt;
    use tracing::Level;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;
    use crate::fake_ekctl::{Fake, fixture, fixture_text};

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const OTHER_ID: &str = "11111111-2222-3333-4444-555555555555";
    const EVENT_ID: &str = "46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076";
    const EVENT_PATH: &str = "/v1/events/46EBD007-078C-44AD-80E9-5D55FDE5FCC8%3A1709076";
    const RANGE: &str = "from=2026-10-05T00:00:00Z&to=2026-10-12T00:00:00Z";

    fn configured() -> Config {
        Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\"]\nwrite_calendar = \"{WRITE_ID}\""
        ))
        .unwrap()
    }

    fn read_only() -> Config {
        Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\"]"
        ))
        .unwrap()
    }

    fn unconfigured() -> Config {
        Config::from_toml("listen = \"127.0.0.1:8790\"").unwrap()
    }

    fn app(config: &Config, runner: Runner) -> Router {
        router(Arc::new(App::new(config, runner)))
    }

    fn show_in(calendar: &str) -> String {
        fixture_text("show_event.json").replace(READ_ID, calendar)
    }

    fn write_fake() -> Fake {
        Fake::scripted(&[
            ("show event", &show_in(WRITE_ID)),
            ("add event", &fixture_text("add_event.json")),
            ("update event", &fixture_text("add_event.json")),
            ("delete event", &fixture_text("delete_event.json")),
        ])
    }

    fn user_event_fake() -> Fake {
        Fake::scripted(&[
            ("show event", &fixture_text("show_event.json")),
            ("update event", &fixture_text("add_event.json")),
            ("delete event", &fixture_text("delete_event.json")),
        ])
    }

    async fn send(
        router: &Router,
        method: Method,
        uri: &str,
        body: Option<&[u8]>,
    ) -> (StatusCode, Value) {
        let body = match body {
            Some(body) => Body::from(body.to_vec()),
            None => Body::empty(),
        };
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body)
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        if bytes.is_empty() {
            return (status, Value::Null);
        }
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
        send(router, Method::GET, uri, None).await
    }

    async fn send_json(
        router: &Router,
        method: Method,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        send(
            router,
            method,
            uri,
            Some(&serde_json::to_vec(&body).unwrap()),
        )
        .await
    }

    fn error(message: &str) -> Value {
        json!({ "error": message })
    }

    fn create_body() -> Value {
        json!({
            "title": "Lunch",
            "start": "2026-02-10T12:30:00Z",
            "end": "2026-02-10T13:30:00Z"
        })
    }

    fn assert_no_writes(fake: &Fake) {
        for call in fake.calls() {
            assert!(
                !call.starts_with("update")
                    && !call.starts_with("delete")
                    && !call.starts_with("add"),
                "{call}"
            );
        }
    }

    #[tokio::test]
    async fn calendars() {
        let fake = Fake::printing("list_calendars.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = get(&router, "/v1/calendars").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"calendars": [
                {"id": READ_ID, "title": "Calendar", "source": "work@example.com", "color": "#0088FF", "writable": false},
                {"id": WRITE_ID, "title": "Agent", "source": "iCloud", "color": "#34C759", "writable": true}
            ]})
        );
    }

    #[tokio::test]
    async fn events_default_to_every_readable_calendar() {
        let fake = Fake::recording("list_events.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = get(&router, &format!("/v1/events?{RANGE}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["events"][0]["id"], json!(EVENT_ID));
        assert_eq!(body["events"][0]["recurring"], json!(true));
        assert_eq!(
            body["events"][0]["start"],
            json!("2026-10-05T11:00:00+02:00")
        );
        assert_eq!(
            fake.recorded_args(),
            vec![
                "list".to_owned(),
                "events".to_owned(),
                format!("--calendar={READ_ID},{WRITE_ID}"),
                "--from=2026-10-05T00:00:00+00:00".to_owned(),
                "--to=2026-10-12T00:00:00+00:00".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn events_for_named_calendar() {
        let fake = Fake::recording("list_events.json");
        let router = app(&configured(), fake.runner());
        let (status, _) = get(&router, &format!("/v1/events?{RANGE}&calendar={WRITE_ID}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fake.recorded_args()[2], format!("--calendar={WRITE_ID}"));
    }

    #[tokio::test]
    async fn events_from_unreadable_calendars_are_dropped() {
        let fake = Fake::printing("list_events.json");
        let router = app(
            &Config::from_toml(&format!(
                "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{OTHER_ID}\"]"
            ))
            .unwrap(),
            fake.runner(),
        );
        let (status, body) = get(&router, &format!("/v1/events?{RANGE}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"events": []}));
    }

    #[tokio::test]
    async fn events_for_unreadable_calendar_are_refused() {
        let fake = Fake::recording("list_events.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = get(
            &router,
            &format!("/v1/events?{RANGE}&calendar={READ_ID}&calendar={OTHER_ID}"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error(&format!("calendar not readable: {OTHER_ID}")));
        assert!(fake.recorded_args().is_empty());
    }

    #[tokio::test]
    async fn events_with_nothing_readable_are_refused() {
        let fake = Fake::recording("list_events.json");
        let router = app(&unconfigured(), fake.runner());
        let (status, body) = get(&router, &format!("/v1/events?{RANGE}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("no readable calendars configured"));
        assert!(fake.recorded_args().is_empty());
    }

    #[tokio::test]
    async fn events_query_validation() {
        let fake = Fake::recording("list_events.json");
        let router = app(&configured(), fake.runner());
        let cases = [
            ("/v1/events", "`from` is required"),
            ("/v1/events?from=2026-10-05T00:00:00Z", "`to` is required"),
            (
                "/v1/events?from=yesterday&to=2026-10-05T00:00:00Z",
                "`from` must be an RFC 3339 timestamp",
            ),
            (
                "/v1/events?from=2026-10-06T00:00:00Z&to=2026-10-05T00:00:00Z",
                "`to` must be after `from`",
            ),
            (
                "/v1/events?from=2026-10-01T00:00:00Z&to=2026-12-03T00:00:00Z",
                "the range must not exceed 62 days",
            ),
            (
                "/v1/events?from=2026-10-05T00:00:00.5Z&to=2026-10-06T00:00:00Z",
                "`from` must not have fractional seconds",
            ),
            ("/v1/events?limit=3", "unknown query parameter `limit`"),
            (
                "/v1/events?calendar=a,b",
                "`calendar` must be a calendar id",
            ),
        ];
        for (uri, message) in cases {
            let (status, body) = get(&router, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(body, error(message), "{uri}");
        }
        assert!(fake.recorded_args().is_empty());
    }

    #[tokio::test]
    async fn show_event() {
        let fake = Fake::recording("show_event.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = get(&router, EVENT_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], json!(EVENT_ID));
        assert_eq!(body["attendees"][0]["email"], json!("a@example.com"));
        assert_eq!(fake.recorded_args(), vec!["show", "event", "--", EVENT_ID]);
    }

    #[tokio::test]
    async fn show_event_decodes_the_path_segment() {
        let fake = Fake::recording("show_event.json");
        let router = app(&configured(), fake.runner());
        let (status, _) = get(&router, "/v1/events/a%2Fb%20c").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fake.recorded_args(), vec!["show", "event", "--", "a/b c"]);
    }

    #[tokio::test]
    async fn show_event_in_unreadable_calendar() {
        let fake = Fake::recording_json(&show_in(OTHER_ID));
        let router = app(&configured(), fake.runner());
        let (status, body) = get(&router, EVENT_PATH).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("event is not in a readable calendar"));
    }

    #[tokio::test]
    async fn show_event_not_found() {
        let fake = Fake::printing("error.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = get(&router, "/v1/events/nonexistent-id").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, error("Event not found with ID: nonexistent-id"));
    }

    #[tokio::test]
    async fn invalid_event_ids() {
        let fake = Fake::recording("show_event.json");
        let router = app(&configured(), fake.runner());
        for method in [Method::GET, Method::PATCH, Method::DELETE] {
            let (status, body) = send(
                &router,
                method.clone(),
                "/v1/events/a%0Ab",
                Some(b"{\"title\":\"x\"}"),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method}");
            assert_eq!(body, error("event id contains a control character"));
            let (status, body) = send(
                &router,
                method.clone(),
                "/v1/events/",
                Some(b"{\"title\":\"x\"}"),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method}");
            assert_eq!(body, error("event id is empty"));
        }
        let (status, body) = get(&router, "/v1/events/%FF").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string());
        assert!(fake.recorded_args().is_empty());
    }

    #[tokio::test]
    async fn free_with_defaults() {
        let fake = Fake::recording("free.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = get(&router, "/v1/free").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
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
        assert_eq!(
            fake.recorded_args(),
            vec![
                "free".to_owned(),
                format!("--calendar={READ_ID},{WRITE_ID}"),
                "--duration=30".to_owned(),
                "--working-hours=09:00-17:00".to_owned(),
                "--weekdays=weekdays".to_owned(),
                "--buffer=0".to_owned(),
                "--limit=20".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn free_with_every_parameter() {
        let fake = Fake::recording("free.json");
        let router = app(&configured(), fake.runner());
        let (status, _) = get(
            &router,
            &format!("/v1/free?duration=60&working_hours=all&weekdays=sat-sun&buffer=10&limit=3&{RANGE}&calendar={READ_ID}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            fake.recorded_args(),
            vec![
                "free".to_owned(),
                format!("--calendar={READ_ID}"),
                "--duration=60".to_owned(),
                "--working-hours=all".to_owned(),
                "--weekdays=saturday,sunday".to_owned(),
                "--buffer=10".to_owned(),
                "--limit=3".to_owned(),
                "--from=2026-10-05T00:00:00+00:00".to_owned(),
                "--to=2026-10-12T00:00:00+00:00".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn free_validation_and_policy() {
        let fake = Fake::recording("free.json");
        let router = app(&configured(), fake.runner());
        let cases = [
            (
                "/v1/free?duration=4",
                "`duration` must be an integer from 5 to 1440",
            ),
            (
                "/v1/free?duration=1441",
                "`duration` must be an integer from 5 to 1440",
            ),
            (
                "/v1/free?buffer=241",
                "`buffer` must be an integer from 0 to 240",
            ),
            (
                "/v1/free?limit=0",
                "`limit` must be an integer from 1 to 100",
            ),
            (
                "/v1/free?limit=101",
                "`limit` must be an integer from 1 to 100",
            ),
            (
                "/v1/free?working_hours=17:00-09:00",
                "`working_hours` must be `all` or HH:MM-HH:MM with start before end",
            ),
            (
                "/v1/free?weekdays=someday",
                "`weekdays` has an unknown day `someday`",
            ),
            (
                "/v1/free?from=2026-10-01T00:00:00Z&to=2026-12-03T00:00:00Z",
                "the range must not exceed 62 days",
            ),
            ("/v1/free?to=soon", "`to` must be an RFC 3339 timestamp"),
        ];
        for (uri, message) in cases {
            let (status, body) = get(&router, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(body, error(message), "{uri}");
        }
        let (status, body) = get(&router, &format!("/v1/free?calendar={OTHER_ID}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error(&format!("calendar not readable: {OTHER_ID}")));
        assert!(fake.recorded_args().is_empty());
    }

    #[tokio::test]
    async fn create_event() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send_json(
            &router,
            Method::POST,
            "/v1/events",
            json!({
                "title": "-Lunch",
                "start": "2026-02-10T12:30:00Z",
                "end": "2026-02-10T14:30:00+01:00",
                "location": "Cafe",
                "notes": "a\nb",
                "url": "https://example.com/"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["calendar"]["id"], json!(WRITE_ID));
        assert_eq!(body["attendees"].as_array().unwrap().len(), 1);
        assert_eq!(
            fake.calls(),
            vec![
                format!(
                    "add event --calendar={WRITE_ID} --title=-Lunch --start=2026-02-10T12:30:00+00:00 --end=2026-02-10T14:30:00+01:00 --location=Cafe --notes=a"
                ),
                "b --url=https://example.com/".to_owned(),
                "show event -- NEW123:EVENT456".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn create_without_write_calendar() {
        let fake = write_fake();
        let router = app(&read_only(), fake.runner());
        let (status, body) = send_json(&router, Method::POST, "/v1/events", create_body()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("no write calendar configured"));
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn create_validation() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let cases = [
            ("title", json!("  "), "`title` must not be blank"),
            (
                "title",
                json!("a".repeat(501)),
                "`title` must be at most 500 characters",
            ),
            (
                "title",
                json!("a\u{7}"),
                "`title` must not contain control characters",
            ),
            (
                "start",
                json!("2026-02-10"),
                "`start` must be an RFC 3339 timestamp",
            ),
            (
                "end",
                json!("2026-02-10T12:30:00Z"),
                "`end` must be after `start`",
            ),
            (
                "location",
                json!("a".repeat(501)),
                "`location` must be at most 500 characters",
            ),
            (
                "notes",
                json!("a".repeat(10_001)),
                "`notes` must be at most 10000 characters",
            ),
            (
                "notes",
                json!("a\rb"),
                "`notes` must not contain control characters",
            ),
            (
                "url",
                json!("ftp://example.com"),
                "`url` must be an absolute http or https URL",
            ),
            (
                "url",
                json!(format!("https://example.com/{}", "a".repeat(2_000))),
                "`url` must be at most 2000 characters",
            ),
        ];
        for (key, value, message) in cases {
            let mut body = create_body();
            body[key] = value;
            let (status, body) = send_json(&router, Method::POST, "/v1/events", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{key}");
            assert_eq!(body, error(message), "{key}");
        }
        let (status, body) = send(&router, Method::POST, "/v1/events", Some(b"{")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("invalid JSON body: ")
        );
        let (status, body) =
            send_json(&router, Method::POST, "/v1/events", json!({"title": "x"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("missing field"));
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn body_over_the_limit() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let mut body = create_body();
        body["notes"] = json!("a".repeat(BODY_LIMIT));
        let (status, body) = send_json(&router, Method::POST, "/v1/events", body).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(body["error"].is_string());
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn update_event() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send_json(
            &router,
            Method::PATCH,
            EVENT_PATH,
            json!({"title": "Renamed", "end": "2026-10-05T12:00:00+02:00"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], json!(EVENT_ID));
        assert_eq!(
            fake.calls(),
            vec![
                format!("show event -- {EVENT_ID}"),
                format!(
                    "update event --title=Renamed --end=2026-10-05T12:00:00+02:00 -- {EVENT_ID}"
                ),
                "show event -- NEW123:EVENT456".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn update_start_checked_against_existing_end() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send_json(
            &router,
            Method::PATCH,
            EVENT_PATH,
            json!({"start": "2026-10-05T11:30:00+02:00"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, error("`end` must be after `start`"));
        assert_eq!(fake.calls(), vec![format!("show event -- {EVENT_ID}")]);
        let (status, _) = send_json(
            &router,
            Method::PATCH,
            EVENT_PATH,
            json!({"start": "2026-10-05T11:29:00+02:00"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_update_is_rejected() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        for body in [json!({}), json!({"notes": null})] {
            let (status, body) = send_json(&router, Method::PATCH, EVENT_PATH, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body, error("the update changes no field"));
        }
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn update_validation() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) =
            send_json(&router, Method::PATCH, EVENT_PATH, json!({"title": ""})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, error("`title` must not be blank"));
        let (status, body) = send_json(
            &router,
            Method::PATCH,
            EVENT_PATH,
            json!({"calendar": WRITE_ID}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("unknown field `calendar`")
        );
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn update_of_a_user_event_is_refused() {
        let fake = user_event_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send_json(
            &router,
            Method::PATCH,
            EVENT_PATH,
            json!({"title": "Mine now"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("event is not in the write calendar"));
        assert_no_writes(&fake);
    }

    #[tokio::test]
    async fn update_without_write_calendar() {
        let fake = write_fake();
        let router = app(&read_only(), fake.runner());
        let (status, body) =
            send_json(&router, Method::PATCH, EVENT_PATH, json!({"title": "x"})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("no write calendar configured"));
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn update_not_found() {
        let fake = Fake::printing("error.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = send_json(
            &router,
            Method::PATCH,
            "/v1/events/nonexistent-id",
            json!({"title": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, error("Event not found with ID: nonexistent-id"));
    }

    #[tokio::test]
    async fn delete_event() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send(&router, Method::DELETE, EVENT_PATH, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(body, Value::Null);
        assert_eq!(
            fake.calls(),
            vec![
                format!("show event -- {EVENT_ID}"),
                format!("delete event -- {EVENT_ID}")
            ]
        );
    }

    #[tokio::test]
    async fn delete_of_a_user_event_is_refused() {
        let fake = user_event_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send(&router, Method::DELETE, EVENT_PATH, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("event is not in the write calendar"));
        assert_no_writes(&fake);
        assert_eq!(fake.calls(), vec![format!("show event -- {EVENT_ID}")]);
    }

    #[tokio::test]
    async fn delete_without_write_calendar_and_not_found() {
        let fake = write_fake();
        let router = app(&read_only(), fake.runner());
        let (status, _) = send(&router, Method::DELETE, EVENT_PATH, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(fake.calls().is_empty());
        let fake = Fake::printing("error.json");
        let router = app(&configured(), fake.runner());
        let (status, _) = send(&router, Method::DELETE, "/v1/events/nonexistent-id", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ekctl_failures_map_to_gateway_errors() {
        let cases = [
            (
                "echo 'denied' >&2\nexit 2",
                StatusCode::BAD_GATEWAY,
                "ekctl exited with code 2: denied",
            ),
            (
                r#"echo '{"status":"error","error":"Calendar not found"}'"#,
                StatusCode::BAD_GATEWAY,
                "ekctl: Calendar not found",
            ),
            (
                "echo garbage",
                StatusCode::BAD_GATEWAY,
                "unexpected ekctl output",
            ),
            (
                "exec head -c 9000000 /dev/zero",
                StatusCode::BAD_GATEWAY,
                "output too large",
            ),
        ];
        for (script, expected, message) in cases {
            let fake = Fake::new(script);
            let router = app(&configured(), fake.runner());
            let (status, body) = get(&router, "/v1/calendars").await;
            assert_eq!(status, expected, "{script}");
            assert_eq!(body, error(message), "{script}");
        }
    }

    #[tokio::test]
    async fn missing_ekctl_is_bad_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let router = app(
            &configured(),
            Runner::new(dir.path().join("ekctl"), Duration::from_secs(5)),
        );
        let (status, body) = get(&router, "/v1/calendars").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("ekctl could not start: ")
        );
    }

    #[tokio::test]
    async fn ekctl_timeout_is_gateway_timeout() {
        let fake = Fake::new("sleep 2");
        let router = app(
            &configured(),
            fake.runner_with_timeout(Duration::from_millis(200)),
        );
        let (status, body) = get(&router, &format!("/v1/events?{RANGE}")).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body, error("ekctl timed out"));
    }

    #[tokio::test]
    async fn healthz_ok() {
        let fake = Fake::printing("list_calendars.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = get(&router, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"status": "ok", "version": env!("CARGO_PKG_VERSION"), "calendars": 2})
        );
    }

    #[tokio::test]
    async fn healthz_degraded() {
        let missing = Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\", \"{OTHER_ID}\"]"
        ))
        .unwrap();
        let cases = [
            (unconfigured(), "cat /dev/null", "unconfigured"),
            (configured(), "sleep 2", "timeout"),
            (configured(), "exit 1", "ekctl failed"),
            (
                missing,
                &format!("cat '{}'", fixture("list_calendars.json").display()) as &str,
                "calendar missing",
            ),
        ];
        for (config, script, reason) in cases {
            let fake = Fake::new(script);
            let router = app(
                &config,
                fake.runner_with_timeout(Duration::from_millis(200)),
            );
            let (status, body) = get(&router, "/healthz").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{reason}");
            assert_eq!(body, json!({"status": "degraded", "reason": reason}));
        }
    }

    #[tokio::test]
    async fn healthz_reuses_the_cached_check() {
        let fake = Fake::new(&format!(
            "echo call >> \"$LOG\"\ncat '{}'",
            fixture("list_calendars.json").display()
        ));
        let router = app(&configured(), fake.runner());
        for _ in 0..3 {
            let (status, _) = get(&router, "/healthz").await;
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(fake.log(), "call\n");
    }

    #[tokio::test]
    async fn unknown_route_and_method() {
        let fake = Fake::printing("list_calendars.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = get(&router, "/v1/reminders").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, error("not found"));
        let (status, body) = send(&router, Method::DELETE, "/v1/calendars", None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body, error("method not allowed"));
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(Level::TRACE)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (captured, guard)
    }

    #[tokio::test]
    async fn request_log_carries_route_status_and_ekctl_but_no_content() {
        let (captured, _guard) = capture();
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, _) = send_json(
            &router,
            Method::POST,
            "/v1/events",
            json!({
                "title": "Secret title",
                "start": "2026-02-10T12:30:00Z",
                "end": "2026-02-10T13:30:00Z",
                "location": "Secret location",
                "notes": "Secret notes",
                "url": "https://secret.example.com/"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = get(&router, EVENT_PATH).await;
        assert_eq!(status, StatusCode::OK);
        let log = captured.text();
        for secret in [
            "Secret",
            "secret.example.com",
            "Standup",
            "long text",
            "Teams",
            "A Person",
            "a@example.com",
            EVENT_ID,
            "1709076",
        ] {
            assert!(!log.contains(secret), "{secret} leaked into:\n{log}");
        }
        assert!(
            log.contains("method=POST route=/v1/events status=201"),
            "{log}"
        );
        assert!(
            log.contains(r#"ekctl="add event=0, show event=0""#),
            "{log}"
        );
        assert!(
            log.contains("method=GET route=/v1/events/{id} status=200"),
            "{log}"
        );
        assert!(log.contains("duration_ms="), "{log}");
    }

    #[tokio::test]
    async fn request_log_without_ekctl_and_unmatched() {
        let (captured, _guard) = capture();
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        get(&router, "/nowhere").await;
        get(&router, "/v1/events").await;
        let log = captured.text();
        assert!(
            log.contains("method=GET route=unmatched status=404"),
            "{log}"
        );
        assert!(
            log.contains("method=GET route=/v1/events status=400"),
            "{log}"
        );
        assert!(!log.contains("ekctl="), "{log}");
    }

    #[tokio::test]
    async fn policy_refusal_logs_its_reason() {
        let (captured, _guard) = capture();
        let fake = user_event_fake();
        let router = app(&configured(), fake.runner());
        send(&router, Method::DELETE, EVENT_PATH, None).await;
        let log = captured.text();
        assert!(
            log.contains(r#"reason=event is not in the write calendar"#),
            "{log}"
        );
        assert!(log.contains(r#"ekctl="show event=0""#), "{log}");
        assert!(log.contains("status=403"), "{log}");
    }

    #[tokio::test]
    async fn timeout_is_logged_as_such() {
        let (captured, _guard) = capture();
        let fake = Fake::new("sleep 2");
        let router = app(
            &configured(),
            fake.runner_with_timeout(Duration::from_millis(100)),
        );
        get(&router, "/v1/calendars").await;
        assert!(
            captured
                .text()
                .contains(r#"ekctl="list calendars=timeout""#),
            "{}",
            captured.text()
        );
    }

    #[tokio::test]
    async fn startup_listing_logs_event_calendars_with_access() {
        let (captured, _guard) = capture();
        let fake = Fake::new(&format!(
            "if [ -e \"$LOG\" ]; then cat '{}'; else touch \"$LOG\"; exit 1; fi",
            fixture("list_calendars.json").display()
        ));
        let app = App::new(&configured(), fake.runner());
        app.announce_calendars(Duration::from_millis(10)).await;
        let log = captured.text();
        assert!(log.contains("cannot list calendars yet"), "{log}");
        assert!(log.contains(&format!("id={READ_ID} title=\"Calendar\" source=\"work@example.com\" readable=true writable=false")), "{log}");
        assert!(
            log.contains(&format!(
                "id={WRITE_ID} title=\"Agent\" source=\"iCloud\" readable=true writable=true"
            )),
            "{log}"
        );
        assert!(!log.contains("Reminders"), "{log}");
    }

    #[tokio::test]
    async fn serve_stops_after_the_grace_period() {
        let fake = Fake::new("sleep 5");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Arc::new(App::new(&configured(), fake.runner()));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            app,
            async move {
                stopped.await.ok();
            },
            Duration::from_millis(300),
        ));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /v1/calendars HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.ok();
    }

    #[tokio::test]
    async fn serve_finishes_in_flight_requests() {
        let fake = Fake::new(&format!(
            "sleep 0.3\ncat '{}'",
            fixture("list_calendars.json").display()
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Arc::new(App::new(&configured(), fake.runner()));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            app,
            async move {
                stopped.await.ok();
            },
            Duration::from_secs(10),
        ));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /v1/calendars HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        time::sleep(Duration::from_millis(100)).await;
        stop.send(()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        server.await.unwrap().unwrap();
    }
}
