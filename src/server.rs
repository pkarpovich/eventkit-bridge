use std::future::{Future, IntoFuture};
use std::io;
use std::mem;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection};
use axum::extract::{DefaultBodyLimit, MatchedPath, Path, RawQuery, Request, State};
use axum::handler::Handler;
use axum::http::uri::Authority;
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Local;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::{self, Instant};

use crate::config::{Config, HostName, Place};
use crate::ekctl::{self, EkctlError};
use crate::health::{HEALTH_TTL, HealthCheck, Probe};
use crate::model::{CalendarKind, EventId, InvalidEventId};
use crate::policy::{GuardError, Policy, PolicyError, ReminderGuardError};
use crate::remindctl::{self, RemindctlError};
use crate::reminders_model::{Conversion, RcReminder, Reminder, ReminderId};
use crate::request::{self, Invalid};

/// The largest request body the bridge accepts.
pub const BODY_LIMIT: usize = 64 * 1024;

/// How long in-flight requests may run after shutdown starts.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(25);

#[derive(Debug)]
struct KnownHosts {
    ip: IpAddr,
    names: Vec<HostName>,
}

impl KnownHosts {
    fn allows(&self, request: &Request) -> bool {
        let mut named = false;
        if let Some(authority) = request.uri().authority() {
            if !self.allows_host(authority.host()) {
                return false;
            }
            named = true;
        }
        for value in request.headers().get_all(header::HOST) {
            let Ok(value) = value.to_str() else {
                return false;
            };
            let Ok(authority) = value.parse::<Authority>() else {
                return false;
            };
            if !self.allows_host(authority.host()) {
                return false;
            }
            named = true;
        }
        named
    }

    fn allows_host(&self, host: &str) -> bool {
        let literal = match host.strip_prefix('[') {
            Some(inner) => inner.strip_suffix(']').unwrap_or(inner),
            None => host,
        };
        if let Ok(ip) = literal.parse::<IpAddr>() {
            return ip.to_canonical() == self.ip.to_canonical();
        }
        let host = host.strip_suffix('.').unwrap_or(host);
        for name in &self.names {
            if name.as_str().eq_ignore_ascii_case(host) {
                return true;
            }
        }
        false
    }
}

/// The runners for the two EventKit CLIs; they share one lock.
#[derive(Debug)]
pub struct Runners {
    /// Runs `ekctl` for calendars.
    pub calendars: ekctl::Runner,
    /// Runs `remindctl` for reminders.
    pub reminders: remindctl::Runner,
}

/// The state every route shares.
#[derive(Debug)]
pub struct App {
    runner: ekctl::Runner,
    reminders: remindctl::Runner,
    policy: Policy,
    places: Vec<Place>,
    health: HealthCheck,
    hosts: KnownHosts,
}

impl App {
    /// The bridge for `config`, running `ekctl` and `remindctl` through `runners`.
    pub fn new(config: &Config, runners: Runners) -> Self {
        let Runners {
            calendars,
            reminders,
        } = runners;
        Self {
            runner: calendars,
            reminders,
            policy: Policy::new(config),
            places: config.places.clone(),
            health: HealthCheck::new(config, HEALTH_TTL),
            hosts: KnownHosts {
                ip: config.listen.ip(),
                names: config.hosts.clone(),
            },
        }
    }

    /// Logs every event calendar with its access once `ekctl list calendars` succeeds,
    /// retrying every `retry` until it does.
    pub async fn announce_calendars(&self, retry: Duration) {
        loop {
            let calendars = self.runner.session().await.list_calendars().await;
            match calendars {
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

    /// Logs every configured place by name, then every reminder list with its access once
    /// `remindctl list` succeeds, retrying every `retry` until it does. Addresses are never logged.
    pub async fn announce_lists(&self, retry: Duration) {
        for Place {
            name,
            address: _,
            radius,
        } in &self.places
        {
            tracing::info!(name = %name, radius, "place");
        }
        loop {
            let lists = self.reminders.session().await.list().await;
            match lists {
                Ok(lists) => {
                    for list in lists {
                        tracing::info!(
                            id = %list.id,
                            title = list.title,
                            readable = self.policy.readable_list(&list.id),
                            writable = self.policy.writable_list(&list.id),
                            "reminder list"
                        );
                    }
                    return;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "cannot list reminder lists yet, retrying");
                    time::sleep(retry).await;
                }
            }
        }
    }

    fn reminder(&self, reminder: RcReminder) -> Reminder {
        reminder.into_reminder(Conversion {
            places: &self.places,
            zone: &Local,
        })
    }
}

#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    Policy(PolicyError),
    Ekctl(EkctlError),
    Remindctl(RemindctlError),
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

impl From<RemindctlError> for ApiError {
    fn from(err: RemindctlError) -> Self {
        ApiError::Remindctl(err)
    }
}

impl From<ReminderGuardError> for ApiError {
    fn from(err: ReminderGuardError) -> Self {
        match err {
            ReminderGuardError::Denied(err) => ApiError::Policy(err),
            ReminderGuardError::Remindctl(err) => ApiError::Remindctl(err),
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
        | EkctlError::Exit { code: _, reason: _ }
        | EkctlError::Reported(_)
        | EkctlError::UnexpectedOutput => StatusCode::BAD_GATEWAY,
    }
}

fn remindctl_status(err: &RemindctlError) -> StatusCode {
    match err {
        RemindctlError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        RemindctlError::NotFound(_) => StatusCode::NOT_FOUND,
        RemindctlError::Spawn(_)
        | RemindctlError::Io(_)
        | RemindctlError::OutputTooLarge
        | RemindctlError::ListNotFound(_)
        | RemindctlError::Exit { code: _, reason: _ }
        | RemindctlError::UnexpectedOutput => StatusCode::BAD_GATEWAY,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            ApiError::Policy(err) => {
                tracing::warn!(reason = %err, "policy refusal");
                (policy_status(&err), err.to_string())
            }
            ApiError::Ekctl(err) => (ekctl_status(&err), err.to_string()),
            ApiError::Remindctl(err) => (remindctl_status(&err), err.to_string()),
            ApiError::Status(status, message) => (status, message),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

fn policy_status(err: &PolicyError) -> StatusCode {
    match err {
        PolicyError::RecurringEvent => StatusCode::CONFLICT,
        PolicyError::CalendarNotReadable(_)
        | PolicyError::NoReadableCalendars
        | PolicyError::EventNotReadable
        | PolicyError::NoWriteCalendar
        | PolicyError::CalendarNotWritable(_)
        | PolicyError::NotInWriteCalendar
        | PolicyError::ListNotReadable(_)
        | PolicyError::NoReadableLists
        | PolicyError::ReminderNotReadable
        | PolicyError::NoWriteList
        | PolicyError::ListNotWritable(_)
        | PolicyError::NotInWriteList => StatusCode::FORBIDDEN,
    }
}

type Shared = State<Arc<App>>;

/// The bridge's HTTP routes, with the body limit, the `Host` check and request logging.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/calendars", get(list_calendars))
        .route(
            "/v1/events",
            get(list_events).post(create_event.layer(middleware::from_fn(require_json))),
        )
        .route(
            "/v1/events/",
            get(empty_event_id)
                .patch(empty_event_id)
                .delete(empty_event_id),
        )
        .route(
            "/v1/events/{id}",
            get(show_event)
                .patch(update_event.layer(middleware::from_fn(require_json)))
                .delete(delete_event),
        )
        .route("/v1/free", get(free))
        .route("/v1/lists", get(list_lists))
        .route("/v1/places", get(list_places))
        .route(
            "/v1/reminders",
            get(list_reminders).post(create_reminder.layer(middleware::from_fn(require_json))),
        )
        .route(
            "/v1/reminders/{id}",
            get(show_reminder)
                .patch(update_reminder.layer(middleware::from_fn(require_json)))
                .delete(delete_reminder),
        )
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            require_known_host,
        ))
        .layer(middleware::from_fn(log_request))
        .with_state(app)
}

/// When the server stops accepting requests, and how long in-flight ones may run after that.
pub struct Shutdown<F> {
    /// Resolves when the server should stop accepting requests.
    pub signal: F,
    /// How long in-flight requests may run once `signal` resolved.
    pub grace: Duration,
}

/// Serves `app` on `listener` until the shutdown signal resolves, then lets in-flight requests
/// finish for at most the grace period.
pub async fn serve<F>(listener: TcpListener, app: Arc<App>, shutdown: Shutdown<F>) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let Shutdown {
        signal: shutdown,
        grace,
    } = shutdown;
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
    let ekctl_calls = ekctl::CallLog::default();
    let remindctl_calls = remindctl::CallLog::default();
    let response = ekctl_calls
        .scope(remindctl_calls.scope(next.run(request)))
        .await;
    let status = response.status().as_u16();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let ekctl = joined(ekctl_calls.calls());
    let remindctl = joined(remindctl_calls.calls());
    match (ekctl, remindctl) {
        (None, None) => tracing::info!(%method, %route, status, duration_ms, "request"),
        (Some(ekctl), None) => {
            tracing::info!(%method, %route, status, duration_ms, ekctl, "request");
        }
        (None, Some(remindctl)) => {
            tracing::info!(%method, %route, status, duration_ms, remindctl, "request");
        }
        (Some(ekctl), Some(remindctl)) => {
            tracing::info!(%method, %route, status, duration_ms, ekctl, remindctl, "request");
        }
    }
    response
}

fn joined<T: ToString>(calls: Vec<T>) -> Option<String> {
    let mut text = String::new();
    for call in calls {
        if !text.is_empty() {
            text.push_str(", ");
        }
        text.push_str(&call.to_string());
    }
    if text.is_empty() {
        return None;
    }
    Some(text)
}

fn event_id(id: &str) -> Result<EventId, ApiError> {
    match EventId::parse(id) {
        Ok(id) => Ok(id),
        Err(err) => Err(ApiError::BadRequest(err.to_string())),
    }
}

fn reminder_id(id: &str) -> Result<ReminderId, ApiError> {
    match ReminderId::parse(id) {
        Ok(id) => Ok(id),
        Err(err) => Err(ApiError::BadRequest(err.to_string())),
    }
}

async fn healthz(State(app): Shared) -> Response {
    app.health
        .check(Probe {
            calendars: &app.runner,
            reminders: &app.reminders,
            policy: &app.policy,
        })
        .await
        .into_response()
}

async fn list_calendars(State(app): Shared) -> Result<Response, ApiError> {
    let calendars = app.runner.session().await.list_calendars().await?;
    let calendars = app.policy.filter_calendars(calendars);
    Ok(Json(json!({ "calendars": calendars })).into_response())
}

async fn list_events(State(app): Shared, RawQuery(query): RawQuery) -> Result<Response, ApiError> {
    let mut range = request::events_query(query.as_deref())?;
    range.calendars = app
        .policy
        .require_readable(mem::take(&mut range.calendars))?;
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

async fn require_known_host(State(app): Shared, request: Request, next: Next) -> Response {
    if !app.hosts.allows(&request) {
        return ApiError::Status(
            StatusCode::MISDIRECTED_REQUEST,
            "unknown host: use the listen IP or a name from `hosts` in the config".to_owned(),
        )
        .into_response();
    }
    next.run(request).await
}

async fn require_json(request: Request, next: Next) -> Response {
    let unsupported = ApiError::Status(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "content type must be application/json".to_owned(),
    );
    let Some(content_type) = request.headers().get(header::CONTENT_TYPE) else {
        return unsupported.into_response();
    };
    let Ok(content_type) = content_type.to_str() else {
        return unsupported.into_response();
    };
    let Some(essence) = content_type.split(';').next() else {
        return unsupported.into_response();
    };
    if !essence.trim().eq_ignore_ascii_case("application/json") {
        return unsupported.into_response();
    }
    next.run(request).await
}

async fn create_event(
    State(app): Shared,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request::CreateRequest { calendar, event } = request::create_body(&body?)?;
    app.policy.require_writable(&calendar)?;
    let session = app.runner.session().await;
    let id = session.add_event(&calendar, &event).await?;
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

async fn list_lists(State(app): Shared) -> Result<Response, ApiError> {
    if !app.policy.any_readable_list() {
        return Ok(Json(json!({ "lists": [] })).into_response());
    }
    let lists = app.reminders.session().await.list().await?;
    let lists = app.policy.filter_lists(lists);
    Ok(Json(json!({ "lists": lists })).into_response())
}

async fn list_places(State(app): Shared) -> Response {
    let mut places = Vec::new();
    for Place {
        name,
        address: _,
        radius,
    } in &app.places
    {
        places.push(json!({"name": name, "radius": radius}));
    }
    Json(json!({ "places": places })).into_response()
}

async fn list_reminders(
    State(app): Shared,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let request::RemindersQuery { status, lists } = request::reminders_query(query.as_deref())?;
    let lists = app.policy.require_readable_lists(lists)?;
    let session = app.reminders.session().await;
    let mut reminders = Vec::new();
    for list in &lists {
        for reminder in session.show(status, list).await? {
            reminders.push(app.reminder(reminder));
        }
    }
    Ok(Json(json!({ "reminders": reminders })).into_response())
}

async fn show_reminder(
    State(app): Shared,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = reminder_id(&id)?;
    let reminder = app.reminders.session().await.info(&id).await?;
    app.policy.require_reminder_readable(&reminder)?;
    Ok(Json(app.reminder(reminder)).into_response())
}

async fn create_reminder(
    State(app): Shared,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let reminder = request::create_reminder_body(&body?, &app.places)?;
    app.policy.require_writable_list(&reminder.list)?;
    let created = app.reminders.session().await.add(&reminder).await?;
    Ok((StatusCode::CREATED, Json(app.reminder(created))).into_response())
}

async fn update_reminder(
    State(app): Shared,
    id: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = reminder_id(&id)?;
    let changes = request::update_reminder_body(&body?)?;
    let session = app.reminders.session().await;
    let existing = app.policy.guard_reminder_write(&session, &id).await?;
    request::merged_repeat(&changes, &existing)?;
    let updated = session.edit(&id, &changes).await?;
    Ok(Json(app.reminder(updated)).into_response())
}

async fn delete_reminder(
    State(app): Shared,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = reminder_id(&id)?;
    let session = app.reminders.session().await;
    app.policy.guard_reminder_write(&session, &id).await?;
    session.delete(&id).await?;
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
    use std::cell::RefCell;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Mutex, Once};

    use axum::body::Body;
    use axum::http::Method;
    use chrono::{DateTime, SecondsFormat};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tower::ServiceExt;
    use tracing::Level;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;
    use crate::ekctl::Runner;
    use crate::fake_ekctl::{Fake, fixture, fixture_text};

    const READ_ID: &str = "4F7D9489-A78F-4369-A951-213207DCFEE3";
    const WRITE_ID: &str = "8C1E2A44-0D6B-4F7E-9C11-5B2F3A9E7D10";
    const OTHER_ID: &str = "11111111-2222-3333-4444-555555555555";
    const EVENT_ID: &str = "46EBD007-078C-44AD-80E9-5D55FDE5FCC8:1709076";
    const EVENT_PATH: &str = "/v1/events/46EBD007-078C-44AD-80E9-5D55FDE5FCC8%3A1709076";
    const RANGE: &str = "from=2026-10-05T00:00:00Z&to=2026-10-12T00:00:00Z";
    const HOST: &str = "127.0.0.1:8790";

    fn configured() -> Config {
        Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\"]\nwrite_calendars = [\"{WRITE_ID}\"]"
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

    fn runners(runner: Runner) -> Runners {
        let reminders = remindctl::Runner::new(
            PathBuf::from("/nonexistent/remindctl"),
            Duration::from_secs(5),
            runner.lock(),
        );
        Runners {
            calendars: runner,
            reminders,
        }
    }

    fn app(config: &Config, runner: Runner) -> Router {
        router(Arc::new(App::new(config, runners(runner))))
    }

    fn show_in(calendar: &str) -> String {
        fixture_text("show_event.json")
            .replace(READ_ID, calendar)
            .replace(
                r#""hasRecurrenceRules":true"#,
                r#""hasRecurrenceRules":false"#,
            )
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

    fn build_request(method: Method, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", HOST)
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    fn json_request(method: Method, uri: &str, body: &Value) -> Request<Body> {
        build_request(method, uri, Body::from(serde_json::to_vec(body).unwrap()))
    }

    async fn send(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        if bytes.is_empty() {
            return (status, Value::Null);
        }
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
        send(router, build_request(Method::GET, uri, Body::empty())).await
    }

    fn error(message: &str) -> Value {
        json!({ "error": message })
    }

    fn create_body() -> Value {
        json!({
            "calendar": WRITE_ID,
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
                build_request(
                    method.clone(),
                    "/v1/events/a%0Ab",
                    Body::from(b"{\"title\":\"x\"}".to_vec()),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method}");
            assert_eq!(body, error("event id contains a control character"));
            let (status, body) = send(
                &router,
                build_request(
                    method.clone(),
                    "/v1/events/",
                    Body::from(b"{\"title\":\"x\"}".to_vec()),
                ),
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
            (
                "/v1/free?to=2099-01-01T00:00:00Z",
                "`from` and `to` must be given together",
            ),
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
        let (status, body) = send(
            &router,
            json_request(
                Method::POST,
                "/v1/events",
                &json!({
                    "calendar": WRITE_ID,
                    "title": "-Lunch",
                    "start": "2026-02-10T12:30:00Z",
                    "end": "2026-02-10T14:30:00+01:00",
                    "location": "Cafe",
                    "notes": "a\nb",
                    "url": "https://example.com/"
                }),
            ),
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
    async fn writes_require_a_json_content_type() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let body = serde_json::to_vec(&create_body()).unwrap();
        let cases = [
            (Method::POST, "/v1/events", Some("text/plain")),
            (Method::POST, "/v1/events", None),
            (
                Method::PATCH,
                EVENT_PATH,
                Some("application/x-www-form-urlencoded"),
            ),
            (Method::PATCH, EVENT_PATH, None),
        ];
        for (method, uri, content_type) in cases {
            let mut request = Request::builder()
                .method(method.clone())
                .uri(uri)
                .header("host", HOST);
            if let Some(content_type) = content_type {
                request = request.header("content-type", content_type);
            }
            let request = request.body(Body::from(body.clone())).unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let message: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{method} {content_type:?}"
            );
            assert_eq!(message, error("content type must be application/json"));
        }
        assert!(fake.calls().is_empty());
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/events")
            .header("host", HOST)
            .header("content-type", "Application/JSON; charset=utf-8")
            .body(Body::from(body))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn create_without_write_calendar() {
        let fake = write_fake();
        let router = app(&read_only(), fake.runner());
        let (status, body) = send(
            &router,
            json_request(Method::POST, "/v1/events", &create_body()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("no write calendars configured"));
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
            let (status, body) =
                send(&router, json_request(Method::POST, "/v1/events", &body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{key}");
            assert_eq!(body, error(message), "{key}");
        }
        let (status, body) = send(
            &router,
            build_request(Method::POST, "/v1/events", Body::from(b"{".to_vec())),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("invalid JSON body: ")
        );
        let (status, body) = send(
            &router,
            json_request(Method::POST, "/v1/events", &json!({"title": "x"})),
        )
        .await;
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
        let (status, body) = send(&router, json_request(Method::POST, "/v1/events", &body)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(body["error"].is_string());
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn update_event() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send(
            &router,
            json_request(
                Method::PATCH,
                EVENT_PATH,
                &json!({"title": "Renamed", "end": "2026-10-05T12:00:00+02:00"}),
            ),
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
        let (status, body) = send(
            &router,
            json_request(
                Method::PATCH,
                EVENT_PATH,
                &json!({"start": "2026-10-05T11:30:00+02:00"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, error("`end` must be after `start`"));
        assert_eq!(fake.calls(), vec![format!("show event -- {EVENT_ID}")]);
        let (status, _) = send(
            &router,
            json_request(
                Method::PATCH,
                EVENT_PATH,
                &json!({"start": "2026-10-05T11:29:00+02:00"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_update_is_rejected() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        for body in [json!({}), json!({"notes": null})] {
            let (status, body) =
                send(&router, json_request(Method::PATCH, EVENT_PATH, &body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body, error("the update changes no field"));
        }
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn update_validation() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send(
            &router,
            json_request(Method::PATCH, EVENT_PATH, &json!({"title": ""})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, error("`title` must not be blank"));
        let (status, body) = send(
            &router,
            json_request(Method::PATCH, EVENT_PATH, &json!({"calendar": WRITE_ID})),
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
        let (status, body) = send(
            &router,
            json_request(Method::PATCH, EVENT_PATH, &json!({"title": "Mine now"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("event is not in a writable calendar"));
        assert_no_writes(&fake);
    }

    #[tokio::test]
    async fn create_in_a_calendar_that_is_not_writable() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let mut body = create_body();
        body["calendar"] = json!(READ_ID);
        let (status, body) = send(&router, json_request(Method::POST, "/v1/events", &body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error(&format!("calendar not writable: {READ_ID}")));
        assert_no_writes(&fake);
    }

    #[tokio::test]
    async fn recurring_events_are_not_changed() {
        let recurring = fixture_text("show_event.json").replace(READ_ID, WRITE_ID);
        let fake = Fake::scripted(&[
            ("show event", &recurring),
            ("update event", &fixture_text("add_event.json")),
            ("delete event", &fixture_text("delete_event.json")),
        ]);
        let router = app(&configured(), fake.runner());
        let refused = error(
            "recurring events cannot be changed: ekctl would change the first occurrence of the series",
        );
        let (status, body) = send(
            &router,
            json_request(Method::PATCH, EVENT_PATH, &json!({"title": "Moved"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body, refused);
        let (status, body) = send(
            &router,
            build_request(Method::DELETE, EVENT_PATH, Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body, refused);
        assert_no_writes(&fake);
    }

    #[tokio::test]
    async fn update_without_write_calendar() {
        let fake = write_fake();
        let router = app(&read_only(), fake.runner());
        let (status, body) = send(
            &router,
            json_request(Method::PATCH, EVENT_PATH, &json!({"title": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("no write calendars configured"));
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn update_not_found() {
        let fake = Fake::printing("error.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = send(
            &router,
            json_request(
                Method::PATCH,
                "/v1/events/nonexistent-id",
                &json!({"title": "x"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, error("Event not found with ID: nonexistent-id"));
    }

    #[tokio::test]
    async fn delete_event() {
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send(
            &router,
            build_request(Method::DELETE, EVENT_PATH, Body::empty()),
        )
        .await;
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

    fn slow_guard_fake() -> Fake {
        Fake::new(&format!(
            r#"case "$1 $2" in
  'show event') echo show >> "$LOG"; sleep 0.3; cat <<'JSON'
{show}
JSON
  ;;
  'update event') echo update >> "$LOG"; cat '{add}' ;;
  'delete event') echo delete >> "$LOG"; cat '{delete}' ;;
  'list calendars') echo list >> "$LOG"; cat '{list}' ;;
esac"#,
            show = show_in(WRITE_ID),
            add = fixture("add_event.json").display(),
            delete = fixture("delete_event.json").display(),
            list = fixture("list_calendars.json").display(),
        ))
    }

    #[tokio::test]
    async fn writes_hold_one_session_across_guard_and_write() {
        let cases = [
            (
                Method::DELETE,
                None,
                StatusCode::NO_CONTENT,
                "show\ndelete\nlist\n",
            ),
            (
                Method::PATCH,
                Some(json!({"title": "Renamed"})),
                StatusCode::OK,
                "show\nupdate\nshow\nlist\n",
            ),
        ];
        for (method, body, expected, log) in cases {
            let fake = slow_guard_fake();
            let router = app(&configured(), fake.runner());
            let write = {
                let router = router.clone();
                let method = method.clone();
                tokio::spawn(async move {
                    match body {
                        Some(body) => send(&router, json_request(method, EVENT_PATH, &body)).await,
                        None => {
                            send(&router, build_request(method, EVENT_PATH, Body::empty())).await
                        }
                    }
                })
            };
            while fake.log().is_empty() {
                time::sleep(Duration::from_millis(10)).await;
            }
            let (status, _) = get(&router, "/v1/calendars").await;
            assert_eq!(status, StatusCode::OK);
            let (status, _) = write.await.unwrap();
            assert_eq!(status, expected, "{method}");
            assert_eq!(fake.log(), log, "{method}");
        }
    }

    #[tokio::test]
    async fn delete_of_a_user_event_is_refused() {
        let fake = user_event_fake();
        let router = app(&configured(), fake.runner());
        let (status, body) = send(
            &router,
            build_request(Method::DELETE, EVENT_PATH, Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("event is not in a writable calendar"));
        assert_no_writes(&fake);
        assert_eq!(fake.calls(), vec![format!("show event -- {EVENT_ID}")]);
    }

    #[tokio::test]
    async fn delete_without_write_calendar_and_not_found() {
        let fake = write_fake();
        let router = app(&read_only(), fake.runner());
        let (status, _) = send(
            &router,
            build_request(Method::DELETE, EVENT_PATH, Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(fake.calls().is_empty());
        let fake = Fake::printing("error.json");
        let router = app(&configured(), fake.runner());
        let (status, _) = send(
            &router,
            build_request(Method::DELETE, "/v1/events/nonexistent-id", Body::empty()),
        )
        .await;
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
        let short = Duration::from_millis(200);
        let usual = Duration::from_secs(10);
        let cases = [
            (unconfigured(), "cat /dev/null", usual, "unconfigured"),
            (configured(), "sleep 2", short, "timeout"),
            (configured(), "exit 1", usual, "ekctl failed"),
            (
                missing,
                &format!("cat '{}'", fixture("list_calendars.json").display()) as &str,
                usual,
                "calendar missing",
            ),
        ];
        for (config, script, timeout, reason) in cases {
            let fake = Fake::new(script);
            let router = app(&config, fake.runner_with_timeout(timeout));
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
        let (status, body) = get(&router, "/v1/tasks").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, error("not found"));
        let (status, body) = send(
            &router,
            build_request(Method::DELETE, "/v1/calendars", Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body, error("method not allowed"));
    }

    async fn send_host(router: &Router, method: Method, hosts: &[&str]) -> StatusCode {
        let mut request = Request::builder().method(method).uri("/v1/events");
        for host in hosts {
            request = request.header("host", *host);
        }
        let request = request
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body()).unwrap()))
            .unwrap();
        router.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn unknown_hosts_are_refused_before_ekctl() {
        let fake = write_fake();
        let config = Config::from_toml(&format!(
            "listen = \"100.64.0.1:8790\"\nhosts = [\"mac.tail1234.ts.net\"]\nwrite_calendars = [\"{WRITE_ID}\"]"
        ))
        .unwrap();
        let router = app(&config, fake.runner());
        let refused: [&[&str]; 7] = [
            &["evil.example.com:8790"],
            &["evil.example.com"],
            &["100.64.0.2:8790"],
            &["localhost:8790"],
            &["100.64.0.1:8790", "evil.example.com:8790"],
            &["not a host"],
            &[],
        ];
        for hosts in refused {
            for method in [Method::GET, Method::POST] {
                let status = send_host(&router, method.clone(), hosts).await;
                assert_eq!(
                    status,
                    StatusCode::MISDIRECTED_REQUEST,
                    "{method} {hosts:?}"
                );
            }
        }
        assert!(fake.calls().is_empty());
        let request = Request::builder()
            .uri("http://evil.example.com:8790/v1/calendars")
            .header("host", "100.64.0.1:8790")
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body,
            error("unknown host: use the listen IP or a name from `hosts` in the config")
        );
        assert!(fake.calls().is_empty());
        for host in [
            "100.64.0.1:8790",
            "100.64.0.1",
            "mac.tail1234.ts.net:8790",
            "MAC.Tail1234.ts.net.",
        ] {
            let status = send_host(&router, Method::POST, &[host]).await;
            assert_eq!(status, StatusCode::CREATED, "{host}");
        }
    }

    #[tokio::test]
    async fn ipv6_listen_host() {
        let fake = Fake::printing("list_calendars.json");
        let config = Config::from_toml(&format!(
            "listen = \"[fd7a:115c:a1e0::1]:8790\"\nread_calendars = [\"{READ_ID}\"]"
        ))
        .unwrap();
        let router = app(&config, fake.runner());
        for (host, expected) in [
            ("[fd7a:115c:a1e0::1]:8790", StatusCode::OK),
            ("[FD7A:115C:A1E0:0::1]", StatusCode::OK),
            ("[fd7a:115c:a1e0::2]:8790", StatusCode::MISDIRECTED_REQUEST),
        ] {
            let request = Request::builder()
                .uri("/v1/calendars")
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            let status = router.clone().oneshot(request).await.unwrap().status();
            assert_eq!(status, expected, "{host}");
        }
    }

    #[tokio::test]
    async fn refused_host_is_logged_without_the_host() {
        let (captured, _guard) = capture();
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        send_host(&router, Method::POST, &["rebound.example.com:8790"]).await;
        let log = captured.text();
        assert!(
            log.contains("method=POST route=/v1/events status=421"),
            "{log}"
        );
        assert!(!log.contains("rebound"), "{log}");
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

    thread_local! {
        static SINK: RefCell<Option<Captured>> = const { RefCell::new(None) };
    }

    struct ThreadSink;

    impl<'a> MakeWriter<'a> for ThreadSink {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            SINK.with_borrow(|sink| sink.clone().unwrap_or_default())
        }
    }

    struct CaptureGuard;

    impl Drop for CaptureGuard {
        fn drop(&mut self) {
            SINK.set(None);
        }
    }

    fn capture() -> (Captured, CaptureGuard) {
        static SUBSCRIBER: Once = Once::new();
        SUBSCRIBER.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(ThreadSink)
                .with_ansi(false)
                .with_max_level(Level::TRACE)
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
        });
        tracing::callsite::rebuild_interest_cache();
        let captured = Captured::default();
        SINK.set(Some(captured.clone()));
        (captured, CaptureGuard)
    }

    #[tokio::test]
    async fn request_log_carries_route_status_and_ekctl_but_no_content() {
        let (captured, _guard) = capture();
        let fake = write_fake();
        let router = app(&configured(), fake.runner());
        let (status, _) = send(
            &router,
            json_request(
                Method::POST,
                "/v1/events",
                &json!({
                    "calendar": WRITE_ID,
                    "title": "Secret title",
                    "start": "2026-02-10T12:30:00Z",
                    "end": "2026-02-10T13:30:00Z",
                    "location": "Secret location",
                    "notes": "Secret notes",
                    "url": "https://secret.example.com/"
                }),
            ),
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
        send(
            &router,
            build_request(Method::DELETE, EVENT_PATH, Body::empty()),
        )
        .await;
        let log = captured.text();
        assert!(
            log.contains(r#"reason=event is not in a writable calendar"#),
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
        let app = App::new(&configured(), runners(fake.runner()));
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
    async fn startup_retry_does_not_hold_the_ekctl_lock() {
        let (_captured, _guard) = capture();
        let fake = Fake::new(&format!(
            "if [ -s \"$LOG\" ]; then cat '{}'; else echo failed >> \"$LOG\"; exit 1; fi",
            fixture("list_calendars.json").display()
        ));
        let app = Arc::new(App::new(&configured(), runners(fake.runner())));
        let announcer = tokio::spawn({
            let app = Arc::clone(&app);
            async move { app.announce_calendars(Duration::from_secs(30)).await }
        });
        while fake.log().is_empty() {
            time::sleep(Duration::from_millis(10)).await;
        }
        let router = router(Arc::clone(&app));
        let (status, _) = time::timeout(Duration::from_secs(5), get(&router, "/v1/calendars"))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        announcer.abort();
    }

    #[tokio::test]
    async fn serve_stops_after_the_grace_period() {
        let fake = Fake::new("sleep 5");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Arc::new(App::new(&configured(), runners(fake.runner())));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            app,
            Shutdown {
                signal: async move {
                    stopped.await.ok();
                },
                grace: Duration::from_millis(300),
            },
        ));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /v1/calendars HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n")
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
    }

    #[tokio::test]
    async fn serve_finishes_in_flight_requests() {
        let fake = Fake::new(&format!(
            "echo start >> \"$LOG\"\nsleep 0.3\ncat '{}'",
            fixture("list_calendars.json").display()
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Arc::new(App::new(&configured(), runners(fake.runner())));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            app,
            Shutdown {
                signal: async move {
                    stopped.await.ok();
                },
                grace: Duration::from_secs(10),
            },
        ));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                b"GET /v1/calendars HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        while fake.log().is_empty() {
            time::sleep(Duration::from_millis(10)).await;
        }
        stop.send(()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        server.await.unwrap().unwrap();
    }

    const REMINDER_ID: &str = "1B2C3D4E-5F6A-4B7C-9D8E-0F1A2B3C4D5E";
    const REMINDER_PATH: &str = "/v1/reminders/1B2C3D4E-5F6A-4B7C-9D8E-0F1A2B3C4D5E";
    const SHOP_ADDRESS: &str = "1 Example Street, Exampletown";

    fn lists_config() -> Config {
        Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\nread_calendars = [\"{READ_ID}\"]\nread_lists = [\"{READ_ID}\"]\nwrite_lists = [\"{WRITE_ID}\"]\n[places]\nshop = {{ address = \"{SHOP_ADDRESS}\", radius = 150 }}\nhome = {{ address = \"2 Home Lane, Hometown\" }}\n"
        ))
        .unwrap()
    }

    fn read_lists_only() -> Config {
        Config::from_toml(&format!(
            "listen = \"127.0.0.1:8790\"\nread_lists = [\"{READ_ID}\"]"
        ))
        .unwrap()
    }

    fn reminder_app(config: &Config, fake: &Fake) -> Router {
        let calendars = fake.runner();
        let reminders = fake.remindctl_runner(calendars.lock());
        router(Arc::new(App::new(
            config,
            Runners {
                calendars,
                reminders,
            },
        )))
    }

    fn info_in(list: &str) -> String {
        fixture_text("remindctl_info.json").replace(WRITE_ID, list)
    }

    fn reminders_fake_with_info(info: &str) -> Fake {
        Fake::scripted(&[
            ("list --json", &fixture_text("remindctl_list.json")),
            ("show open", &fixture_text("remindctl_show.json")),
            ("show completed", &fixture_text("remindctl_show.json")),
            ("show all", &fixture_text("remindctl_show.json")),
            ("info --json", info),
            ("add --json", &fixture_text("remindctl_add.json")),
            ("edit --json", &fixture_text("remindctl_edit.json")),
            ("delete --json", &fixture_text("remindctl_delete.json")),
            ("status --json", &fixture_text("remindctl_status.json")),
            ("list calendars", &fixture_text("list_calendars.json")),
        ])
    }

    fn reminders_fake() -> Fake {
        reminders_fake_with_info(&info_in(WRITE_ID))
    }

    fn reminder_body() -> Value {
        json!({"list": WRITE_ID, "title": "Eggs"})
    }

    fn local(utc: &str) -> String {
        DateTime::parse_from_rfc3339(utc)
            .unwrap()
            .with_timezone(&Local)
            .to_rfc3339_opts(SecondsFormat::Secs, false)
    }

    fn local_day(utc: &str) -> String {
        DateTime::parse_from_rfc3339(utc)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m-%d")
            .to_string()
    }

    fn info_call() -> String {
        format!("info --json --no-input -- {REMINDER_ID}")
    }

    fn assert_no_reminder_writes(fake: &Fake) {
        for call in fake.calls() {
            assert!(
                !call.starts_with("add")
                    && !call.starts_with("edit")
                    && !call.starts_with("delete"),
                "{call}"
            );
        }
    }

    #[tokio::test]
    async fn reminder_lists() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = get(&router, "/v1/lists").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"lists": [
                {"id": WRITE_ID, "title": "Shopping", "open": 4, "writable": true},
                {"id": READ_ID, "title": "Personal", "open": 2, "writable": false}
            ]})
        );
        assert_eq!(fake.calls(), vec!["list --json --no-input"]);
    }

    #[tokio::test]
    async fn places_are_names_and_radii_only() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = get(&router, "/v1/places").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"places": [
                {"name": "home", "radius": 100},
                {"name": "shop", "radius": 150}
            ]})
        );
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn reminders_default_to_open_in_every_readable_list() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = get(&router, "/v1/reminders").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            fake.calls(),
            vec![
                format!("show open --json --no-input --list-id={READ_ID}"),
                format!("show open --json --no-input --list-id={WRITE_ID}"),
            ]
        );
        let reminders = body["reminders"].as_array().unwrap();
        assert_eq!(reminders.len(), 10);
        assert_eq!(
            reminders[0],
            json!({
                "id": "0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D",
                "title": "Milk",
                "notes": null,
                "completed": false,
                "completed_at": null,
                "due": null,
                "all_day": false,
                "repeat": null,
                "priority": "none",
                "list": {"id": WRITE_ID, "title": "Shopping"},
                "location": null,
            })
        );
        assert_eq!(reminders[1]["due"], json!(local("2026-10-06T07:00:00Z")));
        assert_eq!(reminders[1]["repeat"], json!("monthly"));
        assert_eq!(
            reminders[2]["due"],
            json!(local_day("2026-10-06T22:00:00Z"))
        );
        assert_eq!(reminders[2]["all_day"], json!(true));
        assert_eq!(
            reminders[3]["location"],
            json!({"place": "shop", "proximity": "arriving"})
        );
        assert_eq!(reminders[3]["repeat"], json!("custom"));
        assert_eq!(
            reminders[4]["location"],
            json!({"place": null, "proximity": "leaving"})
        );
        assert_eq!(
            reminders[4]["completed_at"],
            json!(local("2026-10-04T16:45:10Z"))
        );
        let text = body.to_string();
        for secret in [
            "Example Street",
            "Elsewhere",
            "50.000",
            "latitude",
            "radius",
        ] {
            assert!(!text.contains(secret), "{secret} returned in {text}");
        }
    }

    #[tokio::test]
    async fn reminders_for_named_lists_and_status() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, _) = get(
            &router,
            &format!("/v1/reminders?status=completed&list={WRITE_ID}&list={WRITE_ID}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(&router, &format!("/v1/reminders?list={READ_ID}&status=all")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            fake.calls(),
            vec![
                format!("show completed --json --no-input --list-id={WRITE_ID}"),
                format!("show all --json --no-input --list-id={READ_ID}"),
            ]
        );
    }

    #[tokio::test]
    async fn reminders_query_validation_and_policy() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let cases = [
            (
                "/v1/reminders?status=done",
                "`status` must be open, completed or all",
            ),
            (
                "/v1/reminders?status=open&status=all",
                "`status` is given more than once",
            ),
            ("/v1/reminders?list=1", "`list` must be a list id"),
            ("/v1/reminders?list=8C1E2A44", "`list` must be a list id"),
            ("/v1/reminders?limit=3", "unknown query parameter `limit`"),
        ];
        for (uri, message) in cases {
            let (status, body) = get(&router, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(body, error(message), "{uri}");
        }
        let (status, body) = get(&router, &format!("/v1/reminders?list={OTHER_ID}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error(&format!("list not readable: {OTHER_ID}")));
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn reminders_off_never_run_remindctl() {
        let fake = reminders_fake();
        let router = reminder_app(&configured(), &fake);
        let (status, body) = get(&router, "/v1/lists").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"lists": []}));
        let (status, body) = get(&router, "/v1/reminders").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("no readable lists configured"));
        let (status, body) = send(
            &router,
            json_request(Method::POST, "/v1/reminders", &reminder_body()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("no write lists configured"));
        for method in [Method::PATCH, Method::DELETE] {
            let (status, body) = send(
                &router,
                json_request(method.clone(), REMINDER_PATH, &json!({"title": "x"})),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
            assert_eq!(body, error("no write lists configured"), "{method}");
        }
        let (status, body) = get(&router, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.get("lists"), None);
        assert_eq!(fake.calls(), vec!["list calendars"]);
    }

    #[tokio::test]
    async fn show_reminder() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = get(&router, REMINDER_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], json!(REMINDER_ID));
        assert_eq!(body["title"], json!("Pay for the box"));
        assert_eq!(body["due"], json!(local("2026-10-06T07:00:00Z")));
        assert_eq!(body["repeat"], json!("weekly"));
        assert_eq!(fake.calls(), vec![info_call()]);
    }

    #[tokio::test]
    async fn show_reminder_with_a_place() {
        let fake = reminders_fake_with_info(&fixture_text("remindctl_info_location.json"));
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = get(&router, REMINDER_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["location"],
            json!({"place": "shop", "proximity": "leaving"})
        );
        assert!(!body.to_string().contains("Example Street"), "{body}");
    }

    #[tokio::test]
    async fn show_reminder_in_unreadable_list() {
        let fake = reminders_fake_with_info(&info_in(OTHER_ID));
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = get(&router, REMINDER_PATH).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("reminder is not in a readable list"));
    }

    fn not_found_fake() -> Fake {
        Fake::new(&format!(
            "printf '%s\\n' \"$*\" >> \"$LOG\"\necho 'Reminder not found: \"{REMINDER_ID}\".' >&2\nexit 1"
        ))
    }

    #[tokio::test]
    async fn reminder_not_found() {
        let fake = not_found_fake();
        let router = reminder_app(&lists_config(), &fake);
        let not_found = error(&format!("Reminder not found: \"{REMINDER_ID}\"."));
        let (status, body) = get(&router, REMINDER_PATH).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, not_found);
        let (status, body) = send(
            &router,
            json_request(Method::PATCH, REMINDER_PATH, &json!({"title": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, not_found);
        let (status, body) = send(
            &router,
            build_request(Method::DELETE, REMINDER_PATH, Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, not_found);
        assert_eq!(fake.calls(), vec![info_call(), info_call(), info_call()]);
    }

    #[tokio::test]
    async fn invalid_reminder_ids() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        for path in [
            "/v1/reminders/1",
            "/v1/reminders/1B2C3D4E",
            "/v1/reminders/--force",
            "/v1/reminders/1B2C3D4E-5F6A-4B7C-9D8E-0F1A2B3C4D5E%0A",
        ] {
            for method in [Method::GET, Method::PATCH, Method::DELETE] {
                let (status, body) = send(
                    &router,
                    json_request(method.clone(), path, &json!({"title": "x"})),
                )
                .await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {path}");
                assert_eq!(
                    body,
                    error("reminder id must be a full UUID"),
                    "{method} {path}"
                );
            }
        }
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn create_reminder_minimal() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = send(
            &router,
            json_request(Method::POST, "/v1/reminders", &reminder_body()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["title"], json!("Eggs"));
        assert_eq!(body["list"], json!({"id": WRITE_ID, "title": "Shopping"}));
        assert_eq!(
            fake.calls(),
            vec![format!(
                "add --json --no-input --title=Eggs --list-id={WRITE_ID}"
            )]
        );
    }

    #[tokio::test]
    async fn create_reminder_with_every_field() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, _) = send(
            &router,
            json_request(
                Method::POST,
                "/v1/reminders",
                &json!({
                    "list": WRITE_ID,
                    "title": "-Eggs",
                    "notes": "--json",
                    "due": "2026-10-06T09:00:00+02:00",
                    "repeat": "weekly",
                    "priority": "high",
                    "place": "shop",
                    "proximity": "leaving"
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = send(
            &router,
            json_request(
                Method::POST,
                "/v1/reminders",
                &json!({
                    "list": WRITE_ID,
                    "title": "Eggs",
                    "due": "2026-10-07",
                    "repeat": "biweekly",
                    "priority": "none",
                    "place": "home"
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            fake.calls(),
            vec![
                format!(
                    "add --json --no-input --title=-Eggs --list-id={WRITE_ID} --notes=--json --due=2026-10-06T09:00:00+02:00 --repeat=weekly --priority=high --location={SHOP_ADDRESS} --radius=150 --leaving"
                ),
                format!(
                    "add --json --no-input --title=Eggs --list-id={WRITE_ID} --due=2026-10-07 --repeat=biweekly --priority=none --location=2 Home Lane, Hometown --radius=100"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn create_reminder_validation() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let cases = [
            ("title", json!(" \n "), "`title` must not be blank"),
            (
                "title",
                json!("a".repeat(501)),
                "`title` must be at most 500 characters",
            ),
            (
                "title",
                json!("a\u{1b}b"),
                "`title` must not contain control characters",
            ),
            (
                "notes",
                json!("a".repeat(10_001)),
                "`notes` must be at most 10000 characters",
            ),
            (
                "notes",
                json!("a\u{0}b"),
                "`notes` must not contain control characters",
            ),
            ("list", json!("1"), "`list` must be a list id"),
            (
                "due",
                json!("tomorrow"),
                "`due` must be an RFC 3339 timestamp or YYYY-MM-DD",
            ),
            (
                "due",
                json!("2026-10-06T09:00:00"),
                "`due` must be an RFC 3339 timestamp or YYYY-MM-DD",
            ),
            (
                "due",
                json!("2026-13-01"),
                "`due` must be an RFC 3339 timestamp or YYYY-MM-DD",
            ),
            (
                "due",
                json!("2026-10-06T09:00:00.5+02:00"),
                "`due` must not have fractional seconds",
            ),
            ("repeat", json!("weekly"), "`repeat` needs `due`"),
            (
                "priority",
                json!("urgent"),
                "`priority` must be none, low, medium or high",
            ),
            (
                "place",
                json!("office"),
                "`place` must be a configured place name",
            ),
            ("proximity", json!("arriving"), "`proximity` needs `place`"),
        ];
        for (key, value, message) in cases {
            let mut body = reminder_body();
            body[key] = value;
            let (status, body) =
                send(&router, json_request(Method::POST, "/v1/reminders", &body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{key}");
            assert_eq!(body, error(message), "{key}");
        }
        let bodies = [
            (
                json!({"list": WRITE_ID, "title": "x", "due": "2026-10-07", "repeat": "hourly"}),
                "`repeat` must be daily, weekly, biweekly, monthly or yearly",
            ),
            (
                json!({"list": WRITE_ID, "title": "x", "place": "shop", "proximity": "near"}),
                "`proximity` must be arriving or leaving",
            ),
        ];
        for (body, message) in bodies {
            let (status, body) =
                send(&router, json_request(Method::POST, "/v1/reminders", &body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{message}");
            assert_eq!(body, error(message));
        }
        let (status, body) = send(
            &router,
            json_request(
                Method::POST,
                "/v1/reminders",
                &json!({"list": WRITE_ID, "title": "x", "url": "https://example.com/"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("unknown field `url`"),
            "{body}"
        );
        let (status, body) = send(
            &router,
            json_request(Method::POST, "/v1/reminders", &json!({"title": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("missing field"));
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn create_reminder_in_a_list_that_is_not_writable() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        for list in [READ_ID, OTHER_ID] {
            let (status, body) = send(
                &router,
                json_request(
                    Method::POST,
                    "/v1/reminders",
                    &json!({"list": list, "title": "Eggs"}),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{list}");
            assert_eq!(body, error(&format!("list not writable: {list}")));
        }
        let router = reminder_app(&read_lists_only(), &fake);
        let (status, body) = send(
            &router,
            json_request(Method::POST, "/v1/reminders", &reminder_body()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, error("no write lists configured"));
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn reminder_writes_require_a_json_content_type() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        for (method, uri) in [
            (Method::POST, "/v1/reminders"),
            (Method::PATCH, REMINDER_PATH),
        ] {
            let request = Request::builder()
                .method(method.clone())
                .uri(uri)
                .header("host", HOST)
                .header("content-type", "text/plain")
                .body(Body::from(serde_json::to_vec(&reminder_body()).unwrap()))
                .unwrap();
            let (status, body) = send(&router, request).await;
            assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{method}");
            assert_eq!(body, error("content type must be application/json"));
        }
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn reminder_routes_refuse_unknown_hosts_and_large_bodies() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let routes = [
            (Method::GET, "/v1/lists"),
            (Method::GET, "/v1/places"),
            (Method::GET, "/v1/reminders"),
            (Method::POST, "/v1/reminders"),
            (Method::GET, REMINDER_PATH),
            (Method::PATCH, REMINDER_PATH),
            (Method::DELETE, REMINDER_PATH),
        ];
        for (method, uri) in routes {
            let request = Request::builder()
                .method(method.clone())
                .uri(uri)
                .header("host", "evil.example.com:8790")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&reminder_body()).unwrap()))
                .unwrap();
            let (status, body) = send(&router, request).await;
            assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{method} {uri}");
            assert_eq!(
                body,
                error("unknown host: use the listen IP or a name from `hosts` in the config"),
                "{method} {uri}"
            );
        }
        for (method, uri) in [
            (Method::POST, "/v1/reminders"),
            (Method::PATCH, REMINDER_PATH),
        ] {
            let mut body = reminder_body();
            body["notes"] = json!("a".repeat(BODY_LIMIT));
            let (status, response) = send(&router, json_request(method.clone(), uri, &body)).await;
            assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{method}");
            assert!(response["error"].is_string(), "{method}");
        }
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn update_reminder() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = send(
            &router,
            json_request(
                Method::PATCH,
                REMINDER_PATH,
                &json!({"title": "-Renamed", "completed": true}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["completed"], json!(true));
        assert_eq!(body["completed_at"], json!(local("2026-10-05T15:25:02Z")));
        assert_eq!(
            fake.calls(),
            vec![
                info_call(),
                format!("edit --json --no-input --title=-Renamed --complete -- {REMINDER_ID}"),
            ]
        );
    }

    #[tokio::test]
    async fn update_reminder_maps_each_field() {
        let cases = [
            (json!({"due": "2026-10-07"}), "--due=2026-10-07"),
            (
                json!({"due": "2026-10-06T09:00:00Z"}),
                "--due=2026-10-06T09:00:00+00:00",
            ),
            (
                json!({"due": null, "repeat": null}),
                "--clear-due --no-repeat",
            ),
            (json!({"repeat": "yearly"}), "--repeat=yearly"),
            (json!({"repeat": null}), "--no-repeat"),
            (json!({"priority": "low"}), "--priority=low"),
            (json!({"notes": "Bring\tbag"}), "--notes=Bring\tbag"),
            (json!({"completed": false}), "--incomplete"),
        ];
        for (body, argv) in cases {
            let fake = reminders_fake();
            let router = reminder_app(&lists_config(), &fake);
            let (status, _) =
                send(&router, json_request(Method::PATCH, REMINDER_PATH, &body)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                fake.calls(),
                vec![
                    info_call(),
                    format!("edit --json --no-input {argv} -- {REMINDER_ID}"),
                ],
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn update_reminder_keeps_repeat_with_a_due_date() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = send(
            &router,
            json_request(Method::PATCH, REMINDER_PATH, &json!({"due": null})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, error("`repeat` needs `due`"));
        assert_eq!(fake.calls(), vec![info_call()]);
        let undated = fixture_text("remindctl_add.json")
            .replace(r#""dueDate" : "2026-10-06T07:00:00Z","#, "");
        let fake = reminders_fake_with_info(&undated);
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = send(
            &router,
            json_request(Method::PATCH, REMINDER_PATH, &json!({"repeat": "daily"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, error("`repeat` needs `due`"));
        assert_no_reminder_writes(&fake);
        let (status, _) = send(
            &router,
            json_request(Method::PATCH, REMINDER_PATH, &json!({"title": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn update_reminder_validation() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let cases = [
            (json!({}), "the update changes no field"),
            (
                json!({"title": null, "notes": null}),
                "the update changes no field",
            ),
            (json!({"title": ""}), "`title` must not be blank"),
            (
                json!({"priority": "urgent"}),
                "`priority` must be none, low, medium or high",
            ),
            (
                json!({"due": "2026-10-06T09:00:00.25Z"}),
                "`due` must not have fractional seconds",
            ),
            (
                json!({"repeat": "hourly"}),
                "`repeat` must be daily, weekly, biweekly, monthly or yearly",
            ),
            (
                json!({"notes": "a\rb"}),
                "`notes` must not contain control characters",
            ),
        ];
        for (body, message) in cases {
            let (status, response) =
                send(&router, json_request(Method::PATCH, REMINDER_PATH, &body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(response, error(message), "{body}");
        }
        for (body, fragment) in [
            (json!({"list": WRITE_ID}), "unknown field `list`"),
            (json!({"place": "shop"}), "unknown field `place`"),
            (json!({"completed": "yes"}), "invalid JSON body"),
        ] {
            let (status, response) =
                send(&router, json_request(Method::PATCH, REMINDER_PATH, &body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(
                response["error"].as_str().unwrap().contains(fragment),
                "{response}"
            );
        }
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn writes_to_a_reminder_outside_the_write_lists_are_refused() {
        for list in [READ_ID, OTHER_ID] {
            let fake = reminders_fake_with_info(&info_in(list));
            let router = reminder_app(&lists_config(), &fake);
            let (status, body) = send(
                &router,
                json_request(Method::PATCH, REMINDER_PATH, &json!({"title": "Mine now"})),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{list}");
            assert_eq!(body, error("reminder is not in a writable list"));
            let (status, body) = send(
                &router,
                build_request(Method::DELETE, REMINDER_PATH, Body::empty()),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{list}");
            assert_eq!(body, error("reminder is not in a writable list"));
            assert_eq!(fake.calls(), vec![info_call(), info_call()]);
        }
    }

    #[tokio::test]
    async fn reminder_writes_without_write_lists() {
        let fake = reminders_fake();
        let router = reminder_app(&read_lists_only(), &fake);
        let (status, _) = send(
            &router,
            json_request(Method::PATCH, REMINDER_PATH, &json!({"title": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = send(
            &router,
            build_request(Method::DELETE, REMINDER_PATH, Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn delete_reminder() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = send(
            &router,
            build_request(Method::DELETE, REMINDER_PATH, Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(body, Value::Null);
        assert_eq!(
            fake.calls(),
            vec![
                info_call(),
                format!("delete --json --no-input --force -- {REMINDER_ID}"),
            ]
        );
    }

    fn slow_reminder_guard_fake() -> Fake {
        Fake::new(&format!(
            r#"case "$1" in
  info) echo info >> "$LOG"; sleep 0.3; cat <<'JSON'
{info}
JSON
  ;;
  edit) echo edit >> "$LOG"; cat '{edit}' ;;
  delete) echo delete >> "$LOG"; cat '{delete}' ;;
  list) echo list >> "$LOG"; cat '{list}' ;;
esac"#,
            info = info_in(WRITE_ID),
            edit = fixture("remindctl_edit.json").display(),
            delete = fixture("remindctl_delete.json").display(),
            list = fixture("list_calendars.json").display(),
        ))
    }

    #[tokio::test]
    async fn reminder_writes_hold_one_session_across_guard_and_write() {
        let cases = [
            (
                Method::DELETE,
                None,
                StatusCode::NO_CONTENT,
                "info\ndelete\nlist\n",
            ),
            (
                Method::PATCH,
                Some(json!({"title": "Renamed"})),
                StatusCode::OK,
                "info\nedit\nlist\n",
            ),
        ];
        for (method, body, expected, log) in cases {
            let fake = slow_reminder_guard_fake();
            let router = reminder_app(&lists_config(), &fake);
            let write = {
                let router = router.clone();
                let method = method.clone();
                tokio::spawn(async move {
                    match body {
                        Some(body) => {
                            send(&router, json_request(method, REMINDER_PATH, &body)).await
                        }
                        None => {
                            send(&router, build_request(method, REMINDER_PATH, Body::empty())).await
                        }
                    }
                })
            };
            while fake.log().is_empty() {
                time::sleep(Duration::from_millis(10)).await;
            }
            let (status, _) = get(&router, "/v1/calendars").await;
            assert_eq!(status, StatusCode::OK);
            let (status, _) = write.await.unwrap();
            assert_eq!(status, expected, "{method}");
            assert_eq!(fake.log(), log, "{method}");
        }
    }

    #[tokio::test]
    async fn remindctl_failures_map_to_gateway_errors() {
        let cases = [
            (
                format!("echo 'List not found: \"{WRITE_ID}\".' >&2\nexit 1"),
                StatusCode::BAD_GATEWAY,
                format!("remindctl: List not found: \"{WRITE_ID}\"."),
            ),
            (
                "echo 'access denied' >&2\nexit 1".to_owned(),
                StatusCode::BAD_GATEWAY,
                "remindctl exited with code 1: access denied".to_owned(),
            ),
            (
                "echo garbage".to_owned(),
                StatusCode::BAD_GATEWAY,
                "unexpected remindctl output".to_owned(),
            ),
        ];
        for (script, expected, message) in cases {
            let fake = Fake::new(&script);
            let router = reminder_app(&lists_config(), &fake);
            let (status, body) = get(&router, "/v1/reminders").await;
            assert_eq!(status, expected, "{script}");
            assert_eq!(body, error(&message), "{script}");
        }
        let fake = Fake::new("sleep 2");
        let calendars = fake.runner();
        let reminders = remindctl::Runner::new(
            fake.program().to_path_buf(),
            Duration::from_millis(200),
            calendars.lock(),
        );
        let router = router(Arc::new(App::new(
            &lists_config(),
            Runners {
                calendars,
                reminders,
            },
        )));
        let (status, body) = get(&router, "/v1/lists").await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body, error("remindctl timed out"));
    }

    #[tokio::test]
    async fn healthz_with_lists() {
        let fake = reminders_fake();
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = get(&router, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"status": "ok", "version": env!("CARGO_PKG_VERSION"), "calendars": 1, "lists": 2})
        );
        assert_eq!(
            fake.calls(),
            vec![
                "list calendars",
                "status --json --no-input",
                "list --json --no-input"
            ]
        );
    }

    #[tokio::test]
    async fn healthz_reminders_access_missing() {
        let fake = Fake::scripted(&[
            ("list calendars", &fixture_text("list_calendars.json")),
            (
                "status --json",
                r#"{"authorized": false, "status": "denied"}"#,
            ),
        ]);
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = get(&router, "/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({"status": "degraded", "reason": "reminders access missing"})
        );
    }

    #[tokio::test]
    async fn reminder_request_log_carries_remindctl_but_no_content() {
        let (captured, _guard) = capture();
        let fake = reminders_fake_with_info(&fixture_text("remindctl_info_location.json"));
        let router = reminder_app(&lists_config(), &fake);
        let (status, _) = send(
            &router,
            json_request(
                Method::POST,
                "/v1/reminders",
                &json!({
                    "list": WRITE_ID,
                    "title": "Secret title",
                    "notes": "Secret notes",
                    "due": "2026-10-06T09:00:00+02:00",
                    "place": "shop"
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = get(&router, "/v1/reminders").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(&router, REMINDER_PATH).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            &router,
            json_request(
                Method::PATCH,
                REMINDER_PATH,
                &json!({"title": "Secret rename", "notes": "Secret change"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(&router, "/v1/places").await;
        assert_eq!(status, StatusCode::OK);
        let log = captured.text();
        for secret in [
            "Secret",
            "Example Street",
            "Exampletown",
            "Home Lane",
            "Elsewhere",
            "50.000",
            "10.000",
            "latitude",
            "Milk",
            "Pay for the box",
            "Example notes",
            "Batteries",
            "Bread",
            "Eggs",
            REMINDER_ID,
        ] {
            assert!(!log.contains(secret), "{secret} leaked into:\n{log}");
        }
        assert!(
            log.contains(r#"method=POST route=/v1/reminders status=201"#),
            "{log}"
        );
        assert!(log.contains(r#"remindctl="add=0""#), "{log}");
        assert!(log.contains(r#"remindctl="show=0, show=0""#), "{log}");
        assert!(log.contains(r#"remindctl="info=0, edit=0""#), "{log}");
        assert!(
            log.contains("method=GET route=/v1/reminders/{id} status=200"),
            "{log}"
        );
        assert!(!log.contains("ekctl="), "{log}");
    }

    #[tokio::test]
    async fn reminder_not_found_is_logged_with_its_exit_code() {
        let (captured, _guard) = capture();
        let fake = not_found_fake();
        let router = reminder_app(&lists_config(), &fake);
        get(&router, REMINDER_PATH).await;
        let log = captured.text();
        assert!(log.contains(r#"remindctl="info=1""#), "{log}");
        assert!(log.contains("status=404"), "{log}");
    }

    #[tokio::test]
    async fn startup_listing_logs_lists_and_place_names() {
        let (captured, _guard) = capture();
        let fake = Fake::new(&format!(
            "if [ -e \"$LOG\" ]; then cat '{}'; else touch \"$LOG\"; exit 1; fi",
            fixture("remindctl_list.json").display()
        ));
        let calendars = fake.runner();
        let reminders = fake.remindctl_runner(calendars.lock());
        let app = App::new(
            &lists_config(),
            Runners {
                calendars,
                reminders,
            },
        );
        app.announce_lists(Duration::from_millis(10)).await;
        let log = captured.text();
        assert!(log.contains("cannot list reminder lists yet"), "{log}");
        assert!(
            log.contains(&format!(
                "id={WRITE_ID} title=\"Shopping\" readable=true writable=true"
            )),
            "{log}"
        );
        assert!(
            log.contains(&format!(
                "id={READ_ID} title=\"Personal\" readable=true writable=false"
            )),
            "{log}"
        );
        assert!(
            log.contains(&format!(
                "id={OTHER_ID} title=\"Work\" readable=false writable=false"
            )),
            "{log}"
        );
        assert!(log.contains("name=home radius=100"), "{log}");
        assert!(log.contains("name=shop radius=150"), "{log}");
        assert!(!log.contains("Example Street"), "{log}");
        assert!(!log.contains("Home Lane"), "{log}");
    }
}
