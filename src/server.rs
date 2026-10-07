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
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use axum::{Json, Router};
use chrono::Local;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::{self, Instant};

use crate::auth::{AuthError, Authenticator, EXEMPT_ROUTE, Principal};
use crate::config::{AccountName, Config, HostName, Place};
use crate::ekctl::{self, EkctlError};
use crate::health::{AuthStatus, HEALTH_TTL, HealthCheck, Probe};
use crate::mail::MessageId;
use crate::mail::emlx::EmlxError;
use crate::mail::script::{self, JunkMove, JunkStatus, Moved, ScriptError};
use crate::mail::store::{
    AccountListing, MailReader, MailStore, MessagePlace, MoveTarget, RegisteredAccount, StoreError,
    Visibility,
};
use crate::model::{CalendarKind, EventId, InvalidEventId};
use crate::policy::{GuardError, Policy, PolicyError, ReminderGuardError};
use crate::remindctl::{self, RemindctlError};
use crate::reminders_model::{RcReminder, Reminder, ReminderId};
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

/// The runners for the two EventKit CLIs, which share one lock, and for Mail's `osascript`.
#[derive(Debug)]
pub struct Runners {
    /// Runs `ekctl` for calendars.
    pub calendars: ekctl::Runner,
    /// Runs `remindctl` for reminders.
    pub reminders: remindctl::Runner,
    /// Runs `osascript` to mark mail junk or not junk.
    pub mail: script::Runner,
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
    mail: Option<Arc<MailStore>>,
    mail_script: script::Runner,
    auth: Option<Arc<Authenticator>>,
}

#[derive(Debug, Clone, Copy)]
struct Rows(usize);

#[derive(Debug, Clone)]
struct Client(String);

#[derive(Debug)]
struct JunkRequest {
    place: MessagePlace,
    id: MessageId,
    status: JunkStatus,
}

#[derive(Debug, Clone)]
struct JunkWrite {
    account: AccountName,
    status: JunkStatus,
}

impl App {
    /// The bridge for `config`, running `ekctl` and `remindctl` through `runners` and checking
    /// bearer tokens with `auth` when `[auth]` is configured.
    pub fn new(config: &Config, runners: Runners, auth: Option<Arc<Authenticator>>) -> Self {
        let Runners {
            calendars,
            reminders,
            mail: mail_script,
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
            mail: config
                .mail
                .clone()
                .map(|mail| Arc::new(MailStore::new(mail))),
            mail_script,
            auth,
        }
    }

    /// Logs every account in Mail's mailboxes table with its counts and configured name, once
    /// the store is readable, retrying every `retry` until it is. Does nothing when `[mail]` is
    /// absent. No message content is logged.
    pub async fn announce_mail(&self, retry: Duration) {
        let Some(store) = &self.mail else {
            return;
        };
        loop {
            let store = Arc::clone(store);
            let listing = tokio::task::spawn_blocking(move || store.open()?.listing()).await;
            let error = match listing {
                Ok(Ok(accounts)) => {
                    for account in accounts {
                        log_mail_account(account);
                    }
                    return;
                }
                Ok(Err(err)) => err.to_string(),
                Err(err) => err.to_string(),
            };
            tracing::warn!(error, "cannot read the mail store yet, retrying");
            time::sleep(retry).await;
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
        reminder.into_reminder(&self.places, &Local)
    }
}

fn log_mail_account(account: AccountListing) {
    let AccountListing {
        id,
        kind,
        registered,
        mailboxes,
        messages,
        newest,
        name,
    } = account;
    let RegisteredAccount {
        account_type,
        description,
    } = registered.unwrap_or_default();
    tracing::info!(
        id = %id,
        kind = %kind,
        account_type,
        description,
        mailboxes,
        messages,
        newest = newest.map(|newest| newest.to_string()),
        configured = name.map(|name| name.to_string()),
        "mail account"
    );
}

#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    Policy(PolicyError),
    Ekctl(EkctlError),
    Remindctl(RemindctlError),
    Mail(StoreError),
    MailScript(ScriptError),
    Status(StatusCode, String),
}

impl From<ScriptError> for ApiError {
    fn from(err: ScriptError) -> Self {
        ApiError::MailScript(err)
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        ApiError::Mail(err)
    }
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
            ApiError::Mail(err) => mail_failure(err),
            ApiError::MailScript(err) => mail_script_failure(err),
            ApiError::Status(status, message) => (status, message),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

fn mail_failure(err: StoreError) -> (StatusCode, String) {
    match err {
        StoreError::UnknownAccount(_) | StoreError::UnknownMailbox(_) => {
            (StatusCode::BAD_REQUEST, err.to_string())
        }
        StoreError::Root(_)
        | StoreError::RootAccess { path: _, source: _ }
        | StoreError::Open(_) => {
            tracing::warn!(error = %err, "cannot open the mail store");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "cannot open the mail store".to_owned(),
            )
        }
        StoreError::Query(_) => {
            tracing::warn!(error = %err, "mail store query failed");
            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        }
        StoreError::File(err) => {
            let reason = match err {
                EmlxError::Io { path: _, source } => source.kind().to_string(),
                EmlxError::OutsideRoot(_) => "outside the mail root".to_owned(),
                EmlxError::LengthLine => "no length line".to_owned(),
            };
            tracing::warn!(reason, "cannot read a message file");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot read the message file: {reason}"),
            )
        }
    }
}

fn mail_script_failure(err: ScriptError) -> (StatusCode, String) {
    tracing::warn!(error = %err, "Mail call failed");
    match err {
        ScriptError::NotPermitted => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Mail automation not permitted: allow EventKitBridge to control Mail in System Settings > Privacy & Security > Automation".to_owned(),
        ),
        ScriptError::NotFound => (StatusCode::NOT_FOUND, "message not found".to_owned()),
        ScriptError::Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "Mail did not answer".to_owned(),
        ),
        ScriptError::Spawn(_)
        | ScriptError::Io(_)
        | ScriptError::OutputTooLarge
        | ScriptError::Failed { code: _ }
        | ScriptError::UnexpectedOutput => (StatusCode::BAD_GATEWAY, "Mail failed".to_owned()),
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

fn routes() -> Vec<(&'static str, MethodRouter<Arc<App>>)> {
    vec![
        ("/healthz", get(healthz)),
        ("/v1/calendars", get(list_calendars)),
        (
            "/v1/events",
            get(list_events).post(create_event.layer(middleware::from_fn(require_json))),
        ),
        (
            "/v1/events/",
            get(empty_event_id)
                .patch(empty_event_id)
                .delete(empty_event_id),
        ),
        (
            "/v1/events/{id}",
            get(show_event)
                .patch(update_event.layer(middleware::from_fn(require_json)))
                .delete(delete_event),
        ),
        ("/v1/free", get(free)),
        ("/v1/lists", get(list_lists)),
        ("/v1/places", get(list_places)),
        (
            "/v1/reminders",
            get(list_reminders).post(create_reminder.layer(middleware::from_fn(require_json))),
        ),
        (
            "/v1/reminders/{id}",
            get(show_reminder)
                .patch(update_reminder.layer(middleware::from_fn(require_json)))
                .delete(delete_reminder),
        ),
        ("/v1/mail/accounts", get(mail_accounts)),
        ("/v1/mail/messages", get(mail_messages)),
        (
            "/v1/mail/messages/{id}",
            get(mail_message).patch(mark_mail_junk.layer(middleware::from_fn(require_json))),
        ),
    ]
}

/// The bridge's HTTP routes, with request logging, the `Host` check, the bearer-token check
/// and the body limit, in that order.
pub fn router(app: Arc<App>) -> Router {
    let mut router = Router::new();
    for (path, route) in routes() {
        router = router.route(path, route);
    }
    router
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            require_bearer,
        ))
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
    let rows = response.extensions().get::<Rows>().map(|Rows(rows)| *rows);
    let (account, junk) = match response.extensions().get::<JunkWrite>() {
        Some(JunkWrite { account, status }) => (Some(account.to_string()), Some(status.is_junk())),
        None => (None, None),
    };
    let client = response
        .extensions()
        .get::<Client>()
        .map(|Client(client)| client.clone());
    tracing::info!(
        %method,
        %route,
        status,
        duration_ms,
        ekctl,
        remindctl,
        rows,
        account,
        junk,
        client = client.as_ref().map(tracing::field::display),
        "request"
    );
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
            mail: app.mail.as_ref(),
            auth: app.auth.as_deref().map(|auth| AuthStatus::of(auth.jwks())),
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

async fn require_bearer(State(app): Shared, request: Request, next: Next) -> Response {
    let Some(auth) = &app.auth else {
        return next.run(request).await;
    };
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());
    if route.as_deref() == Some(EXEMPT_ROUTE) {
        return next.run(request).await;
    }
    let principal = match auth.authenticate(request.headers()).await {
        Ok(principal) => principal,
        Err(err) => return refuse_token(err),
    };
    let required = match &route {
        Some(route) => auth.required_scope(request.method(), route),
        None => None,
    };
    let Principal { client, scopes: _ } = &principal;
    if let Some(scope) = required
        && !principal.has_scope(&scope)
    {
        tracing::warn!(kind = "insufficient scope", "token refused");
        return with_client(insufficient_scope(&scope), client.clone());
    }
    with_client(next.run(request).await, client.clone())
}

fn with_client(mut response: Response, client: String) -> Response {
    response.extensions_mut().insert(Client(client));
    response
}

fn refuse_token(err: AuthError) -> Response {
    tracing::warn!(kind = %err, "token refused");
    let (message, challenge) = match err {
        AuthError::Missing => ("missing bearer token", "Bearer"),
        AuthError::Malformed | AuthError::UnknownKey | AuthError::Invalid => {
            ("invalid bearer token", r#"Bearer error="invalid_token""#)
        }
    };
    challenged(
        StatusCode::UNAUTHORIZED,
        message.to_owned(),
        HeaderValue::from_static(challenge),
    )
}

fn insufficient_scope(scope: &str) -> Response {
    let challenge = format!(r#"Bearer error="insufficient_scope", scope="{scope}""#);
    let challenge = HeaderValue::from_str(&challenge)
        .unwrap_or_else(|_| HeaderValue::from_static(r#"Bearer error="insufficient_scope""#));
    challenged(
        StatusCode::FORBIDDEN,
        format!("insufficient scope: {scope} needed"),
        challenge,
    )
}

fn challenged(status: StatusCode, message: String, challenge: HeaderValue) -> Response {
    let mut response = ApiError::Status(status, message).into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, challenge);
    response
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
            if app.policy.readable_list(&reminder.list_id) {
                reminders.push(app.reminder(reminder));
            }
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
    if !app.policy.any_readable_list() {
        return Err(PolicyError::NoReadableLists.into());
    }
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

fn mail_store(app: &App) -> Result<Arc<MailStore>, ApiError> {
    let Some(store) = &app.mail else {
        return Err(ApiError::Status(
            StatusCode::NOT_FOUND,
            "mail is off: add [mail] to the config".to_owned(),
        ));
    };
    Ok(Arc::clone(store))
}

async fn read_mail<T, F>(store: Arc<MailStore>, read: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&MailReader<'_>) -> Result<T, StoreError> + Send + 'static,
{
    let result = tokio::task::spawn_blocking(move || read(&store.open()?)).await;
    match result {
        Ok(result) => Ok(result?),
        Err(err) => {
            tracing::warn!(error = %err, "mail read did not finish");
            Err(ApiError::Status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "mail read failed".to_owned(),
            ))
        }
    }
}

fn with_rows(rows: usize, response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response.extensions_mut().insert(Rows(rows));
    response
}

async fn mail_accounts(State(app): Shared) -> Result<Response, ApiError> {
    let store = mail_store(&app)?;
    let accounts = read_mail(store, |reader| reader.accounts()).await?;
    Ok(with_rows(
        accounts.len(),
        Json(json!({ "accounts": accounts })),
    ))
}

async fn mail_messages(
    State(app): Shared,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let store = mail_store(&app)?;
    let query = request::mail_messages_query(query.as_deref())?;
    let page = read_mail(store, move |reader| reader.messages(&query)).await?;
    Ok(with_rows(page.messages.len(), Json(page)))
}

async fn mail_message(
    State(app): Shared,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let store = mail_store(&app)?;
    let Path(id) = id?;
    let id = request::mail_message_id(&id)?;
    let message = read_mail(store, move |reader| reader.message(id)).await?;
    let Some(message) = message else {
        return Err(ApiError::Status(
            StatusCode::NOT_FOUND,
            "message not found".to_owned(),
        ));
    };
    Ok(with_rows(1, Json(message)))
}

async fn mark_mail_junk(
    State(app): Shared,
    id: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let store = mail_store(&app)?;
    let Path(id) = id?;
    let id = request::mail_message_id(&id)?;
    let status = request::mail_junk_body(&body?)?;
    let place = read_mail(store, move |reader| reader.place(id)).await?;
    let Some(place) = place else {
        return Err(ApiError::Status(
            StatusCode::NOT_FOUND,
            "message not found".to_owned(),
        ));
    };
    let logged = JunkWrite {
        account: place.account.name.clone(),
        status,
    };
    let request = JunkRequest { place, id, status };
    let mut response = match move_junk(&app.mail_script, request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    };
    response.extensions_mut().insert(logged);
    Ok(response)
}

async fn move_junk(runner: &script::Runner, request: JunkRequest) -> Result<Response, ApiError> {
    let JunkRequest { place, id, status } = request;
    let MessagePlace {
        account,
        mailbox,
        junk,
        inbox,
    } = place;
    let (target, missing) = match status {
        JunkStatus::Junk => (junk, "account has no junk mailbox"),
        JunkStatus::NotJunk => (inbox, "account has no inbox"),
    };
    let Some(MoveTarget { path, visibility }) = target else {
        return Err(ApiError::Status(StatusCode::CONFLICT, missing.to_owned()));
    };
    let request = JunkMove {
        account: account.id,
        source: mailbox,
        id,
        target: path,
        status,
    };
    let Moved(moved) = runner.junk(&request).await?;
    let id = match visibility {
        Visibility::Visible => moved,
        Visibility::Excluded => None,
    };
    Ok(Json(json!({
        "id": id,
        "account": account.name,
        "mailbox": request.target,
        "junk": status.is_junk(),
    }))
    .into_response())
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
    use crate::auth::test_keys;
    use crate::auth::{scope_for, test_source};
    use crate::config::AuthConfig;
    use crate::ekctl::Runner;
    use crate::fake_ekctl::{Fake, fixture, fixture_text};
    use crate::mail::fixture::{self as mail_fixture, Fixture as MailFixture};

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
            mail: no_osascript(),
        }
    }

    fn no_osascript() -> script::Runner {
        script::Runner::new(
            PathBuf::from("/nonexistent/osascript"),
            Duration::from_secs(5),
        )
    }

    fn app(config: &Config, runner: Runner) -> Router {
        router(Arc::new(App::new(config, runners(runner), None)))
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

    struct Sinks(Vec<Captured>);

    impl Write for Sinks {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            for sink in &mut self.0 {
                sink.write_all(buf)?;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    thread_local! {
        static SINK: RefCell<Option<Captured>> = const { RefCell::new(None) };
    }

    static POOL_SINKS: Mutex<Vec<Captured>> = Mutex::new(Vec::new());

    struct ThreadSink;

    impl<'a> MakeWriter<'a> for ThreadSink {
        type Writer = Sinks;

        fn make_writer(&'a self) -> Self::Writer {
            let sink = SINK.with_borrow(Clone::clone);
            if let Some(sink) = sink {
                return Sinks(vec![sink]);
            }
            let thread = std::thread::current();
            let Some(name) = thread.name() else {
                return Sinks(Vec::new());
            };
            if !name.starts_with("tokio-") {
                return Sinks(Vec::new());
            }
            Sinks(POOL_SINKS.lock().unwrap().clone())
        }
    }

    struct CaptureGuard(Captured);

    impl Drop for CaptureGuard {
        fn drop(&mut self) {
            SINK.set(None);
            let Self(Captured(captured)) = self;
            POOL_SINKS
                .lock()
                .unwrap()
                .retain(|Captured(sink)| !Arc::ptr_eq(sink, captured));
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
        POOL_SINKS.lock().unwrap().push(captured.clone());
        (captured.clone(), CaptureGuard(captured))
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
        let app = App::new(&configured(), runners(fake.runner()), None);
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
        let app = Arc::new(App::new(&configured(), runners(fake.runner()), None));
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
        let app = Arc::new(App::new(&configured(), runners(fake.runner()), None));
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
        let app = Arc::new(App::new(&configured(), runners(fake.runner()), None));
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
                mail: no_osascript(),
            },
            None,
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
        assert_eq!(reminders[3]["repeat"], json!("every 3 days"));
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
    async fn reminders_from_unreadable_lists_are_dropped() {
        let fake = Fake::scripted(&[(
            "show open",
            &fixture_text("remindctl_show.json").replace(WRITE_ID, OTHER_ID),
        )]);
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = get(&router, &format!("/v1/reminders?list={WRITE_ID}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"reminders": []}));
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
        let (status, body) = get(&router, REMINDER_PATH).await;
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
                "`repeat` must be daily, weekly, biweekly, monthly, yearly or every N days, weeks, months or years, with N from 2 to 999",
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
    async fn failed_geocode_never_returns_the_place_address() {
        let fake = Fake::new(&format!(
            "echo 'Error: Could not geocode location: {SHOP_ADDRESS}' >&2\nexit 1"
        ));
        let router = reminder_app(&lists_config(), &fake);
        let (status, body) = send(
            &router,
            json_request(
                Method::POST,
                "/v1/reminders",
                &json!({"list": WRITE_ID, "title": "Eggs", "place": "shop"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body,
            error("remindctl exited with code 1: Error: Could not geocode location: place shop")
        );
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
            (
                json!({"due": "2026-10-07"}),
                "--due=2026-10-07 --clear-alarm",
            ),
            (
                json!({"due": "2026-10-06T09:00:00Z"}),
                "--due=2026-10-06T09:00:00+00:00 --alarm=2026-10-06T09:00:00+00:00",
            ),
            (
                json!({"due": null, "repeat": null}),
                "--clear-due --clear-alarm --no-repeat",
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
                "`repeat` must be daily, weekly, biweekly, monthly, yearly or every N days, weeks, months or years, with N from 2 to 999",
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
                mail: no_osascript(),
            },
            None,
        )));
        let (status, body) = get(&router, "/v1/lists").await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body, error("remindctl timed out"));
    }

    #[tokio::test]
    async fn healthz_with_lists() {
        let (captured, _guard) = capture();
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
        let log = captured.text();
        assert!(
            log.contains(r#"ekctl="list calendars=0" remindctl="status=0, list=0""#),
            "{log}"
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
                mail: no_osascript(),
            },
            None,
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
    fn mail_config(fixture: &MailFixture) -> Config {
        let mut config = configured();
        config.mail = Some(fixture.config());
        config
    }

    fn no_ekctl() -> Runner {
        Runner::new(PathBuf::from("/nonexistent/ekctl"), Duration::from_secs(5))
    }

    fn mail_app(fixture: &MailFixture) -> Router {
        app(&mail_config(fixture), no_ekctl())
    }

    fn mail_date(seconds: i64) -> String {
        DateTime::from_timestamp(seconds, 0)
            .unwrap()
            .with_timezone(&Local)
            .to_rfc3339_opts(SecondsFormat::Secs, false)
    }

    fn message_ids(body: &Value) -> Vec<i64> {
        let mut ids = Vec::new();
        for message in body["messages"].as_array().unwrap() {
            ids.push(message["id"].as_i64().unwrap());
        }
        ids
    }

    const ALL_MAIL: [i64; 6] = [
        mail_fixture::MULTIPART_ALL_MAIL,
        mail_fixture::MULTIPART,
        mail_fixture::HTML,
        mail_fixture::PLAIN,
        mail_fixture::PARTIAL,
        mail_fixture::MISSING,
    ];

    #[tokio::test]
    async fn mail_accounts() {
        let fixture = MailFixture::standard();
        let router = mail_app(&fixture);
        let (status, body) = get(&router, "/v1/mail/accounts").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"accounts": [
                {"name": "gmail", "type": "imap", "mailboxes": [
                    {"path": "INBOX", "total": 1, "unread": 1},
                    {"path": "[Gmail]/All Mail", "total": 1, "unread": 1}
                ]},
                {"name": "main", "type": "exchange", "mailboxes": [
                    {"path": "Inbox", "total": 4, "unread": 2}
                ]}
            ]})
        );
    }

    #[tokio::test]
    async fn mail_messages_lists_every_visible_message_newest_first() {
        let fixture = MailFixture::standard();
        let router = mail_app(&fixture);
        let (status, body) = get(&router, "/v1/mail/messages").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(message_ids(&body), ALL_MAIL);
        assert_eq!(body["next_cursor"], Value::Null);
        assert_eq!(
            body["messages"][3],
            json!({
                "id": mail_fixture::PLAIN,
                "account": "main",
                "mailbox": "Inbox",
                "date": mail_date(mail_fixture::T + 100),
                "from": {"name": "Alice Example", "address": "alice@example.com"},
                "to": [{"name": "Bob", "address": "bob@example.com"}],
                "subject": "Quarterly report",
                "summary": "Numbers attached",
                "read": true,
                "flagged": false,
                "has_body": true
            })
        );
        assert_eq!(body["messages"][0]["mailbox"], "[Gmail]/All Mail");
        assert_eq!(body["messages"][1]["mailbox"], "INBOX");
        assert_eq!(body["messages"][5]["has_body"], false);
    }

    #[tokio::test]
    async fn mail_messages_apply_filters() {
        let fixture = MailFixture::standard();
        let router = mail_app(&fixture);
        let since = mail_date(mail_fixture::T + 100).replace('+', "%2B");
        let until = mail_date(mail_fixture::T + 300).replace('+', "%2B");
        let cases: [(String, Vec<i64>); 7] = [
            (
                "account=main".to_owned(),
                vec![
                    mail_fixture::HTML,
                    mail_fixture::PLAIN,
                    mail_fixture::PARTIAL,
                    mail_fixture::MISSING,
                ],
            ),
            (
                "account=gmail&mailbox=inbox".to_owned(),
                vec![mail_fixture::MULTIPART],
            ),
            (
                "account=main&unread=true".to_owned(),
                vec![mail_fixture::HTML, mail_fixture::MISSING],
            ),
            (
                format!("since={since}&until={until}"),
                vec![mail_fixture::HTML, mail_fixture::PLAIN],
            ),
            (
                "q=%D0%BF%D0%A0%D0%98%D0%B2%D0%B5%D1%82".to_owned(),
                vec![mail_fixture::MULTIPART_ALL_MAIL, mail_fixture::MULTIPART],
            ),
            (
                "q=BOB%40example.com&account=main&account=gmail".to_owned(),
                vec![mail_fixture::HTML, mail_fixture::PLAIN],
            ),
            (
                "unread=true&q=deals&mailbox=Inbox".to_owned(),
                vec![mail_fixture::HTML],
            ),
        ];
        for (query, expected) in cases {
            let (status, body) = get(&router, &format!("/v1/mail/messages?{query}")).await;
            assert_eq!(status, StatusCode::OK, "{query}: {body}");
            assert_eq!(message_ids(&body), expected, "{query}");
        }
    }

    #[tokio::test]
    async fn mail_messages_page_with_the_cursor() {
        let fixture = MailFixture::standard();
        let router = mail_app(&fixture);
        let mut seen = Vec::new();
        let mut uri = "/v1/mail/messages?limit=4".to_owned();
        let mut pages = 0;
        loop {
            let (status, body) = get(&router, &uri).await;
            assert_eq!(status, StatusCode::OK);
            seen.extend(message_ids(&body));
            pages += 1;
            let Some(cursor) = body["next_cursor"].as_str() else {
                break;
            };
            uri = format!("/v1/mail/messages?limit=4&cursor={cursor}");
        }
        assert_eq!(pages, 2);
        assert_eq!(seen, ALL_MAIL);
    }

    #[tokio::test]
    async fn mail_messages_validation() {
        let fixture = MailFixture::standard();
        let router = mail_app(&fixture);
        let cases = [
            ("account=nobody", "unknown mail account \"nobody\""),
            ("account=Main", "unknown mail account \"Main\""),
            ("mailbox=Spam", "unknown mailbox \"Spam\""),
            (
                "mailbox=Deleted%20Items",
                "unknown mailbox \"Deleted Items\"",
            ),
            (
                "account=main&mailbox=INBOX%2F..%2F..%2Fetc",
                "unknown mailbox \"INBOX/../../etc\"",
            ),
            (
                "account=gmail&mailbox=Archive",
                "unknown mailbox \"Archive\"",
            ),
            ("mailbox=", "`mailbox` must not be empty"),
            ("since=yesterday", "`since` must be an RFC 3339 timestamp"),
            ("until=2026-10-05", "`until` must be an RFC 3339 timestamp"),
            (
                "since=2026-10-05T00:00:00Z&until=2026-10-04T00:00:00Z",
                "`until` must be after `since`",
            ),
            ("unread=1", "`unread` must be true or false"),
            ("limit=0", "`limit` must be an integer from 1 to 100"),
            ("limit=101", "`limit` must be an integer from 1 to 100"),
            (
                "cursor=%21%21",
                "`cursor` must be the `next_cursor` of a previous page",
            ),
            (
                "cursor=MTc5MTIwMDEwMA",
                "`cursor` must be the `next_cursor` of a previous page",
            ),
            (
                "from=2026-10-05T00:00:00Z",
                "unknown query parameter `from`",
            ),
            ("q=a&q=b", "`q` is given more than once"),
        ];
        for (query, message) in cases {
            let (status, body) = get(&router, &format!("/v1/mail/messages?{query}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
            assert_eq!(body, error(message), "{query}");
        }
        for id in ["abc", "0", "-1", "01", "1.5"] {
            let (status, body) = get(&router, &format!("/v1/mail/messages/{id}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{id}");
            assert_eq!(body, error("message id must be a positive integer"));
        }
    }

    #[tokio::test]
    async fn mail_message() {
        let fixture = MailFixture::standard();
        let router = mail_app(&fixture);
        let (status, body) = get(
            &router,
            &format!("/v1/mail/messages/{}", mail_fixture::PLAIN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], mail_fixture::PLAIN);
        assert_eq!(body["subject"], "Quarterly report");
        assert_eq!(
            body["cc"],
            json!([{"name": "Carol", "address": "carol@example.com"}])
        );
        assert_eq!(body["body"], "Plain version\r\n");
        assert_eq!(body["body_truncated"], false);
        assert_eq!(body["partial"], false);
        assert_eq!(body["attachments"], json!([]));

        let (status, body) = get(
            &router,
            &format!("/v1/mail/messages/{}", mail_fixture::MULTIPART),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["account"], "gmail");
        assert_eq!(body["subject"], mail_fixture::CYRILLIC_SUBJECT);
        assert_eq!(body["body"], mail_fixture::CYRILLIC_BODY);
        assert_eq!(
            body["attachments"],
            json!([{
                "name": "report.pdf",
                "content_type": "application/pdf",
                "size": mail_fixture::ATTACHMENT_BYTES.len()
            }])
        );

        let (status, body) = get(
            &router,
            &format!("/v1/mail/messages/{}", mail_fixture::HTML),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["body"].as_str().unwrap().contains("Big sale"),
            "{body}"
        );
        assert_eq!(body["flagged"], true);

        let (status, body) = get(
            &router,
            &format!("/v1/mail/messages/{}", mail_fixture::PARTIAL),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["partial"], true);
        assert_eq!(body["has_body"], true);

        let (status, body) = get(
            &router,
            &format!("/v1/mail/messages/{}", mail_fixture::MISSING),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["has_body"], false);
        assert_eq!(body["body"], Value::Null);
        assert_eq!(body["partial"], false);
        assert_eq!(body["attachments"], json!([]));
    }

    #[tokio::test]
    async fn invisible_mail_messages_are_not_found() {
        let fixture = MailFixture::standard();
        let router = mail_app(&fixture);
        for id in [
            mail_fixture::IN_DELETED_ITEMS,
            mail_fixture::DELETED_ROW,
            mail_fixture::UNCONFIGURED,
            mail_fixture::IN_SPAM,
            mail_fixture::IN_CRAFTED,
            999_999,
        ] {
            let (status, body) = get(&router, &format!("/v1/mail/messages/{id}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
            assert_eq!(body, error("message not found"), "{id}");
        }
    }

    #[tokio::test]
    async fn mail_off_is_not_found() {
        let router = app(&configured(), no_ekctl());
        for uri in [
            "/v1/mail/accounts",
            "/v1/mail/messages",
            "/v1/mail/messages?limit=0",
            "/v1/mail/messages/830",
            "/v1/mail/messages/abc",
        ] {
            let (status, body) = get(&router, uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(
                body,
                error("mail is off: add [mail] to the config"),
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn mail_routes_refuse_other_methods_and_check_the_host() {
        let fixture = MailFixture::standard();
        let router = mail_app(&fixture);
        for (method, uri) in [
            (Method::POST, "/v1/mail/accounts"),
            (Method::PATCH, "/v1/mail/accounts"),
            (Method::POST, "/v1/mail/messages"),
            (Method::PATCH, "/v1/mail/messages"),
            (Method::POST, "/v1/mail/messages/830"),
            (Method::PUT, "/v1/mail/messages/830"),
            (Method::DELETE, "/v1/mail/messages/830"),
        ] {
            let (status, body) = send(&router, build_request(method, uri, Body::empty())).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{uri}");
            assert_eq!(body, error("method not allowed"));
        }
        for method in [Method::GET, Method::PATCH] {
            let request = Request::builder()
                .method(method)
                .uri("/v1/mail/messages/830")
                .header("host", "rebound.example.com:8790")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"junk": true}"#))
                .unwrap();
            let (status, _) = send(&router, request).await;
            assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
        }
    }

    #[tokio::test]
    async fn mail_store_failures() {
        let fixture = MailFixture::standard();
        let mut config = mail_config(&fixture);
        if let Some(mail) = &mut config.mail {
            mail.root = Some(fixture.root.join("missing"));
        }
        let fake = osascript_fake();
        let router = junk_app(&config, &fake);
        let (status, body) = get(&router, "/v1/mail/messages").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, error("cannot open the mail store"));
        let (status, body) = mark(&router, mail_fixture::PLAIN, true).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, error("cannot open the mail store"));

        std::fs::write(
            fixture.root.join(crate::config::MAIL_INDEX_RELATIVE_PATH),
            b"not a database at all, just text that is long enough to be read as a header",
        )
        .unwrap();
        let router = mail_app(&fixture);
        for uri in [
            "/v1/mail/accounts",
            "/v1/mail/messages",
            "/v1/mail/messages/830",
        ] {
            let (status, body) = get(&router, uri).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap()
                    .starts_with("Envelope Index query failed"),
                "{body}"
            );
        }
        let router = junk_app(&mail_config(&fixture), &fake);
        let (status, body) = mark(&router, mail_fixture::PLAIN, true).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("Envelope Index query failed"),
            "{body}"
        );
        assert_eq!(fake.log(), "");
    }

    #[tokio::test]
    async fn unreadable_message_file_is_a_server_error_without_its_path() {
        let fixture = MailFixture::standard();
        let inbox = fixture.root.join(mail_fixture::MAIN).join("Inbox.mbox");
        let outside = tempfile::tempdir().unwrap();
        std::fs::rename(&inbox, outside.path().join("Inbox.mbox")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("Inbox.mbox"), &inbox).unwrap();
        let router = mail_app(&fixture);
        let (status, body) = get(
            &router,
            &format!("/v1/mail/messages/{}", mail_fixture::PLAIN),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            error("cannot read the message file: outside the mail root")
        );
    }

    #[tokio::test]
    async fn message_file_without_a_length_line_is_a_server_error() {
        let fixture = MailFixture::standard();
        let path = fixture
            .root
            .join(mail_fixture::MAIN)
            .join("Inbox.mbox")
            .join(mail_fixture::STORE)
            .join(crate::mail::emlx::partition(
                crate::mail::MessageId::new(mail_fixture::PLAIN).unwrap(),
            ))
            .join(format!("{}.emlx", mail_fixture::PLAIN));
        std::fs::write(&path, "abc\nSubject: x\r\n\r\ny").unwrap();
        let router = mail_app(&fixture);
        let (status, body) = get(
            &router,
            &format!("/v1/mail/messages/{}", mail_fixture::PLAIN),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, error("cannot read the message file: no length line"));
    }

    #[tokio::test]
    async fn mail_without_configured_accounts_shows_nothing() {
        let fixture = MailFixture::standard();
        let mut config = mail_config(&fixture);
        if let Some(mail) = &mut config.mail {
            mail.accounts = Vec::new();
        }
        let router = app(&config, no_ekctl());
        let (status, body) = get(&router, "/v1/mail/accounts").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"accounts": []}));
        let (status, body) = get(&router, "/v1/mail/messages").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(message_ids(&body), [0_i64; 0]);
        let (status, _) = get(
            &router,
            &format!("/v1/mail/messages/{}", mail_fixture::PLAIN),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn healthz_with_mail() {
        let fixture = MailFixture::standard();
        let fake = Fake::printing("list_calendars.json");
        let router = app(&mail_config(&fixture), fake.runner());
        let (status, body) = get(&router, "/healthz").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["calendars"], 2);
        assert_eq!(body["mail_accounts"], 2);
        assert!(body["newest_message_age_s"].is_u64(), "{body}");

        let (status, body) = get(&router, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("lists").is_none(), "{body}");

        let fixture = MailFixture::empty();
        for (id, url) in [
            (1, format!("ews://{}/Inbox", mail_fixture::MAIN)),
            (2, format!("imap://{}/INBOX", mail_fixture::GMAIL)),
        ] {
            fixture.mailbox(&mail_fixture::MailboxRow {
                id,
                url,
                total: 0,
                unread: 0,
            });
        }
        let router = app(&mail_config(&fixture), fake.runner());
        let (status, body) = get(&router, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["mail_accounts"], 2);
        assert_eq!(body["newest_message_age_s"], Value::Null);
    }

    #[tokio::test]
    async fn healthz_without_mail_has_no_mail_fields() {
        let fake = Fake::printing("list_calendars.json");
        let router = app(&configured(), fake.runner());
        let (status, body) = get(&router, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("mail_accounts").is_none(), "{body}");
        assert!(body.get("newest_message_age_s").is_none(), "{body}");
    }

    #[tokio::test]
    async fn healthz_mail_degraded() {
        let fake = Fake::printing("list_calendars.json");
        let check = async |config: &Config| {
            let router = app(config, fake.runner());
            get(&router, "/healthz").await
        };

        let fixture = MailFixture::standard();
        let mut config = mail_config(&fixture);
        if let Some(mail) = &mut config.mail {
            mail.root = Some(fixture.root.join("missing"));
        }
        let missing_root = check(&config).await;

        let unreadable = MailFixture::standard();
        std::fs::write(
            unreadable
                .root
                .join(crate::config::MAIL_INDEX_RELATIVE_PATH),
            b"not a database at all, just text that is long enough to be read as a header",
        )
        .unwrap();
        let unreadable_index = check(&mail_config(&unreadable)).await;

        let changed = MailFixture::standard();
        changed
            .writer()
            .execute_batch("ALTER TABLE recipients DROP COLUMN position")
            .unwrap();
        let schema_changed = check(&mail_config(&changed)).await;

        let fixture = MailFixture::standard();
        let mut config = mail_config(&fixture);
        if let Some(mail) = &mut config.mail {
            mail.accounts.push(crate::config::MailAccount {
                id: crate::config::AccountId::parse(OTHER_ID).unwrap(),
                name: crate::config::AccountName::parse("gone").unwrap(),
            });
        }
        let account_missing = check(&config).await;

        for ((status, body), reason) in [
            (missing_root, "mail no access"),
            (unreadable_index, "mail no access"),
            (schema_changed, "mail schema changed"),
            (account_missing, "mail account missing"),
        ] {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{reason}");
            assert_eq!(body, json!({"status": "degraded", "reason": reason}));
        }
    }

    #[tokio::test]
    async fn mail_request_log_carries_rows_but_no_content() {
        let (captured, _guard) = capture();
        let fixture = MailFixture::standard();
        let router = mail_app(&fixture);
        let (status, _) = get(&router, "/v1/mail/accounts").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(&router, "/v1/mail/messages").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(
            &router,
            "/v1/mail/messages?q=quarterly%20report&mailbox=Inbox&account=main",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(&router, "/v1/mail/messages?mailbox=Secret%20folder").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        for id in [
            mail_fixture::PLAIN,
            mail_fixture::HTML,
            mail_fixture::MULTIPART,
            mail_fixture::PARTIAL,
            mail_fixture::MISSING,
            mail_fixture::IN_SPAM,
        ] {
            get(&router, &format!("/v1/mail/messages/{id}")).await;
        }
        let inbox = fixture.root.join(mail_fixture::MAIN).join("Inbox.mbox");
        let outside = tempfile::tempdir().unwrap();
        std::fs::rename(&inbox, outside.path().join("Inbox.mbox")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("Inbox.mbox"), &inbox).unwrap();
        let (status, _) = get(&router, "/v1/mail/messages?account=main").await;
        assert_eq!(status, StatusCode::OK);
        let fake = osascript_fake();
        let junk = junk_app(&mail_config(&fixture), &fake);
        let (status, _) = mark(&junk, mail_fixture::MULTIPART, true).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = mark(&junk, mail_fixture::MULTIPART_ALL_MAIL, false).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = mark(&junk, mail_fixture::PLAIN, true).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = mark(&junk, mail_fixture::IN_SPAM, false).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let failing = Fake::new(
            "echo 'execution error: Secret \"[Gmail]/Spam\" <hidden-id@example.com> (-10000)' >&2\nexit 1",
        );
        let junk = junk_app(&mail_config(&fixture), &failing);
        let (status, _) = mark(&junk, mail_fixture::MULTIPART, true).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let log = captured.text();
        assert!(log.contains("message file refused"), "{log}");
        for secret in [
            "Alice",
            "alice@example.com",
            "Bob",
            "bob@example.com",
            "Carol",
            "carol@example.com",
            "Quarterly",
            "quarterly",
            "Numbers attached",
            "Plain version",
            "Newsletter",
            "news@shop.example",
            "Weekly deals",
            "Big sale",
            "Иван",
            "ivan@example.ru",
            "me@gmail.example",
            mail_fixture::CYRILLIC_SUBJECT,
            mail_fixture::CYRILLIC_BODY,
            "report.pdf",
            "Long thread",
            "half downloaded",
            "Subject",
            "sender@example.com",
            "Secret",
            "hidden-id@example.com",
            "Spam",
            "INBOX",
            "All Mail",
            "Gmail",
        ] {
            assert!(!log.contains(secret), "{secret} leaked into:\n{log}");
        }
        for expected in [
            "method=PATCH route=/v1/mail/messages/{id} status=200 duration_ms=",
            r#"account="gmail" junk=true"#,
            r#"account="gmail" junk=false"#,
            "method=PATCH route=/v1/mail/messages/{id} status=409",
            r#"account="main" junk=true"#,
            "method=PATCH route=/v1/mail/messages/{id} status=404",
            "method=PATCH route=/v1/mail/messages/{id} status=502",
            "Mail failed with error -10000",
        ] {
            assert!(log.contains(expected), "{expected} missing from:\n{log}");
        }
        assert!(
            log.contains("method=GET route=/v1/mail/accounts status=200"),
            "{log}"
        );
        assert!(log.contains("rows=2"), "{log}");
        assert!(log.contains("rows=6"), "{log}");
        assert!(
            log.contains("method=GET route=/v1/mail/messages status=400"),
            "{log}"
        );
        assert!(
            log.contains("method=GET route=/v1/mail/messages/{id} status=200"),
            "{log}"
        );
        assert!(
            log.contains("method=GET route=/v1/mail/messages/{id} status=404"),
            "{log}"
        );
        for line in log.lines() {
            if !line.contains("route=/v1/mail/") {
                continue;
            }
            if line.contains("method=GET route=/v1/mail/messages/{id} status=200") {
                assert!(line.ends_with(" rows=1"), "{line}");
            }
            if line.contains("method=PATCH") {
                assert!(!line.contains("rows="), "{line}");
            }
            if line.contains("status=400") || line.contains("status=404") {
                assert!(!line.contains("rows="), "{line}");
            }
        }
        assert!(!log.contains("ekctl="), "{log}");
    }

    #[tokio::test]
    async fn startup_listing_logs_mail_accounts_without_content() {
        let (captured, _guard) = capture();
        let fixture = MailFixture::standard();
        fixture.register_accounts(&[
            (mail_fixture::MAIN, "com.apple.account.Exchange", "Work"),
            (
                mail_fixture::GMAIL,
                "com.apple.account.IMAP",
                "me@gmail.example",
            ),
        ]);
        let app = App::new(&mail_config(&fixture), runners(no_ekctl()), None);
        app.announce_mail(Duration::from_millis(10)).await;
        let log = captured.text();
        assert!(
            log.contains(&format!(
                "id={} kind=exchange account_type=\"com.apple.account.Exchange\" description=\"Work\" mailboxes=2 messages=5 newest=\"{}\" configured=\"main\"",
                mail_fixture::MAIN,
                mail_date(mail_fixture::T + 400)
            )),
            "{log}"
        );
        assert!(
            log.contains(&format!(
                "id={} kind=imap account_type=\"com.apple.account.IMAP\" description=\"me@gmail.example\" mailboxes=3 messages=3",
                mail_fixture::GMAIL
            )),
            "{log}"
        );
        assert!(
            log.contains(&format!(
                "mail account id={} kind=imap mailboxes=1 messages=1 newest=\"{}\"\n",
                mail_fixture::OTHER,
                mail_date(mail_fixture::T + 600)
            )),
            "{log}"
        );
        for secret in [
            "Alice",
            "alice@example.com",
            "Quarterly",
            "Weekly deals",
            mail_fixture::CYRILLIC_SUBJECT,
            "ivan@example.ru",
            "sender@example.com",
            "Inbox",
            "Spam",
        ] {
            assert!(!log.contains(secret), "{secret} leaked into:\n{log}");
        }
    }

    #[tokio::test]
    async fn startup_listing_retries_while_the_mail_store_is_unreadable() {
        let (captured, _guard) = capture();
        let fixture = MailFixture::standard();
        let mut config = mail_config(&fixture);
        if let Some(mail) = &mut config.mail {
            mail.root = Some(fixture.root.join("missing"));
        }
        let app = App::new(&config, runners(no_ekctl()), None);
        let finished = time::timeout(
            Duration::from_millis(200),
            app.announce_mail(Duration::from_millis(10)),
        )
        .await;
        assert!(finished.is_err());
        let log = captured.text();
        assert!(
            log.matches("cannot read the mail store yet, retrying")
                .count()
                >= 2,
            "{log}"
        );

        let app = App::new(&configured(), runners(no_ekctl()), None);
        app.announce_mail(Duration::from_millis(10)).await;
        assert!(!captured.text().contains("mail account"));
    }

    const RECORD_ARGS: &str = "for arg in \"$@\"; do printf '%s\\0' \"$arg\" >> \"$LOG\"; done";
    const MOVED_ID: i64 = 900_001;

    fn osascript_fake() -> Fake {
        Fake::new(&format!(
            "{RECORD_ARGS}\nif [ \"$4\" = \"$6\" ]; then echo \"$5\"; else echo {MOVED_ID}; fi"
        ))
    }

    fn junk_app(config: &Config, fake: &Fake) -> Router {
        junk_app_with_timeout(config, fake, Duration::from_secs(10))
    }

    fn junk_app_with_timeout(config: &Config, fake: &Fake, timeout: Duration) -> Router {
        let mut runners = runners(no_ekctl());
        runners.mail = script::Runner::new(fake.program().to_owned(), timeout);
        router(Arc::new(App::new(config, runners, None)))
    }

    fn without_exclusions(fixture: &MailFixture) -> Config {
        let mut config = mail_config(fixture);
        if let Some(mail) = &mut config.mail {
            mail.exclude_mailboxes = Vec::new();
        }
        config
    }

    async fn mark(router: &Router, id: i64, junk: bool) -> (StatusCode, Value) {
        send(
            router,
            json_request(
                Method::PATCH,
                &format!("/v1/mail/messages/{id}"),
                &json!({ "junk": junk }),
            ),
        )
        .await
    }

    #[tokio::test]
    async fn mark_mail_junk_moves_to_the_junk_mailbox() {
        let fixture = MailFixture::standard();
        let fake = osascript_fake();
        let router = junk_app(&without_exclusions(&fixture), &fake);
        let (status, body) = mark(&router, mail_fixture::MULTIPART, true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"id": MOVED_ID, "account": "gmail", "mailbox": "[Gmail]/Spam", "junk": true})
        );
        assert_eq!(
            fake.recorded_args(),
            [
                "-e",
                script::SCRIPT,
                mail_fixture::GMAIL,
                "INBOX",
                "383621",
                "[Gmail]/Spam",
                "true"
            ]
        );
    }

    #[tokio::test]
    async fn mark_mail_not_junk_moves_to_the_inbox() {
        let fixture = MailFixture::standard();
        let fake = osascript_fake();
        let router = junk_app(&without_exclusions(&fixture), &fake);
        let (status, body) = mark(&router, mail_fixture::IN_SPAM, false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"id": MOVED_ID, "account": "gmail", "mailbox": "INBOX", "junk": false})
        );
        assert_eq!(
            fake.recorded_args(),
            [
                "-e",
                script::SCRIPT,
                mail_fixture::GMAIL,
                "[Gmail]/Spam",
                &mail_fixture::IN_SPAM.to_string(),
                "INBOX",
                "false"
            ]
        );
    }

    #[tokio::test]
    async fn mark_mail_already_in_its_target_passes_the_same_source_and_target() {
        let fixture = MailFixture::standard();
        let fake = osascript_fake();
        let router = junk_app(&mail_config(&fixture), &fake);
        let (status, body) = mark(&router, mail_fixture::PLAIN, false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"id": mail_fixture::PLAIN, "account": "main", "mailbox": "Inbox", "junk": false})
        );
        assert_eq!(
            fake.recorded_args(),
            [
                "-e",
                script::SCRIPT,
                mail_fixture::MAIN,
                "Inbox",
                "830",
                "Inbox",
                "false"
            ]
        );
    }

    #[tokio::test]
    async fn mark_mail_junk_into_an_excluded_mailbox_has_no_id() {
        let fixture = MailFixture::standard();
        let fake = osascript_fake();
        let router = junk_app(&mail_config(&fixture), &fake);
        let (status, body) = mark(&router, mail_fixture::MULTIPART_ALL_MAIL, true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"id": null, "account": "gmail", "mailbox": "[Gmail]/Spam", "junk": true})
        );
        assert_eq!(fake.recorded_args()[3], "[Gmail]/All Mail");
    }

    #[tokio::test]
    async fn mark_mail_whose_copy_did_not_show_up_has_no_id() {
        let fixture = MailFixture::standard();
        let fake = Fake::new("echo");
        let router = junk_app(&mail_config(&fixture), &fake);
        let (status, body) = mark(&router, mail_fixture::MULTIPART_ALL_MAIL, false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"id": null, "account": "gmail", "mailbox": "INBOX", "junk": false})
        );
    }

    #[tokio::test]
    async fn mark_mail_without_a_target_mailbox_is_a_conflict() {
        let fixture = MailFixture::standard();
        let fake = osascript_fake();
        let router = junk_app(&mail_config(&fixture), &fake);
        let (status, body) = mark(&router, mail_fixture::PLAIN, true).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body, error("account has no junk mailbox"));

        let fixture = MailFixture::empty();
        fixture.mailbox(&mail_fixture::MailboxRow {
            id: 1,
            url: format!("imap://{}/%5BGmail%5D/All%20Mail", mail_fixture::GMAIL),
            total: 0,
            unread: 0,
        });
        fixture.insert(&mail_fixture::Row::new(10, 1, mail_fixture::T));
        let router = junk_app(&mail_config(&fixture), &fake);
        let (status, body) = mark(&router, 10, false).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body, error("account has no inbox"));
        let (status, body) = mark(&router, 10, true).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body, error("account has no junk mailbox"));
        assert_eq!(fake.log(), "");
    }

    #[tokio::test]
    async fn mark_invisible_mail_is_not_found() {
        let fixture = MailFixture::standard();
        let fake = osascript_fake();
        let router = junk_app(&mail_config(&fixture), &fake);
        for id in [
            mail_fixture::IN_DELETED_ITEMS,
            mail_fixture::DELETED_ROW,
            mail_fixture::UNCONFIGURED,
            mail_fixture::IN_SPAM,
            mail_fixture::IN_CRAFTED,
            999_999,
        ] {
            for junk in [true, false] {
                let (status, body) = mark(&router, id, junk).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
                assert_eq!(body, error("message not found"), "{id}");
            }
        }
        let mut config = mail_config(&fixture);
        if let Some(mail) = &mut config.mail {
            mail.accounts = Vec::new();
        }
        let router = junk_app(&config, &fake);
        let (status, _) = mark(&router, mail_fixture::PLAIN, true).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(fake.log(), "");
    }

    #[tokio::test]
    async fn mark_mail_maps_mail_failures() {
        let fixture = MailFixture::standard();
        let fail = |stderr: &str| format!("cat >&2 <<'ERR'\n{stderr}\nERR\nexit 1");
        let cases = [
            (
                fail(
                    "0:120: execution error: Not authorized to send Apple events to Mail. (-1743)",
                ),
                StatusCode::SERVICE_UNAVAILABLE,
                "Mail automation not permitted: allow EventKitBridge to control Mail in System Settings > Privacy & Security > Automation",
            ),
            (
                fail(
                    "0:215: execution error: Mail got an error: Can’t get message 1 of mailbox \"INBOX\". (-1728)",
                ),
                StatusCode::NOT_FOUND,
                "message not found",
            ),
            (
                fail("0:301: execution error: Mail got an error: AppleEvent timed out. (-1712)"),
                StatusCode::GATEWAY_TIMEOUT,
                "Mail did not answer",
            ),
            (
                fail(
                    "0:88: execution error: Mail got an error: \"[Gmail]/Spam\" <hidden@example.com> (-10000)",
                ),
                StatusCode::BAD_GATEWAY,
                "Mail failed",
            ),
            (
                fail("osascript: no such file"),
                StatusCode::BAD_GATEWAY,
                "Mail failed",
            ),
            (
                "echo 'missing value'".to_owned(),
                StatusCode::BAD_GATEWAY,
                "Mail failed",
            ),
        ];
        for (script, expected, message) in cases {
            let fake = Fake::new(&script);
            let router = junk_app(&mail_config(&fixture), &fake);
            let (status, body) = mark(&router, mail_fixture::MULTIPART, true).await;
            assert_eq!(status, expected, "{script}");
            assert_eq!(body, error(message), "{script}");
        }

        let fake = Fake::new("sleep 5\necho 1");
        let router =
            junk_app_with_timeout(&mail_config(&fixture), &fake, Duration::from_millis(200));
        let (status, body) = mark(&router, mail_fixture::MULTIPART, true).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body, error("Mail did not answer"));

        let router = mail_app(&fixture);
        let (status, body) = mark(&router, mail_fixture::MULTIPART, true).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body, error("Mail failed"));
    }

    #[tokio::test]
    async fn mark_mail_validates_the_request() {
        let fixture = MailFixture::standard();
        let fake = osascript_fake();
        let router = junk_app(&mail_config(&fixture), &fake);
        let uri = format!("/v1/mail/messages/{}", mail_fixture::PLAIN);
        for body in [
            "",
            "{}",
            "null",
            r#"{"junk": null}"#,
            r#"{"junk": "true"}"#,
            r#"{"junk": 1}"#,
            r#"{"junk": true, "mailbox": "Archive"}"#,
            "[true]",
        ] {
            let (status, response) = send(
                &router,
                build_request(Method::PATCH, &uri, Body::from(body)),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(
                response["error"]
                    .as_str()
                    .unwrap()
                    .starts_with("invalid JSON body: "),
                "{response}"
            );
        }
        for id in ["abc", "0", "-1", "01", "1.5"] {
            let (status, body) = send(
                &router,
                json_request(
                    Method::PATCH,
                    &format!("/v1/mail/messages/{id}"),
                    &json!({"junk": true}),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{id}");
            assert_eq!(body, error("message id must be a positive integer"));
        }
        let request = Request::builder()
            .method(Method::PATCH)
            .uri(&uri)
            .header("host", HOST)
            .body(Body::from(r#"{"junk": true}"#))
            .unwrap();
        let (status, body) = send(&router, request).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(body, error("content type must be application/json"));
        let big = format!(r#"{{"junk": true, "pad": "{}"}}"#, "x".repeat(BODY_LIMIT));
        let (status, _) = send(&router, build_request(Method::PATCH, &uri, Body::from(big))).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(fake.log(), "");
    }

    #[tokio::test]
    async fn mark_mail_when_mail_is_off() {
        let fake = osascript_fake();
        let router = junk_app(&configured(), &fake);
        for id in ["830", "abc"] {
            let (status, body) = send(
                &router,
                json_request(
                    Method::PATCH,
                    &format!("/v1/mail/messages/{id}"),
                    &json!({"junk": true}),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
            assert_eq!(body, error("mail is off: add [mail] to the config"));
        }
        assert_eq!(fake.log(), "");
    }

    #[tokio::test]
    async fn mail_writes_do_not_wait_for_the_store_lock() {
        let fixture = MailFixture::standard();
        let calendars = Fake::new("echo start >> \"$LOG\"\nsleep 5");
        let osascript = osascript_fake();
        let mut runners = runners(calendars.runner());
        runners.mail = script::Runner::new(osascript.program().to_owned(), Duration::from_secs(10));
        let router = router(Arc::new(App::new(&mail_config(&fixture), runners, None)));
        let held = tokio::spawn({
            let router = router.clone();
            async move { get(&router, "/v1/calendars").await }
        });
        while calendars.log().is_empty() {
            time::sleep(Duration::from_millis(10)).await;
        }
        let (status, _) = time::timeout(
            Duration::from_secs(3),
            mark(&router, mail_fixture::MULTIPART_ALL_MAIL, false),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK);
        held.abort();
    }

    #[tokio::test]
    async fn concurrent_mail_writes_reach_mail_one_at_a_time() {
        let fixture = MailFixture::standard();
        let fake = Fake::new("echo start >> \"$LOG\"\nsleep 0.2\necho end >> \"$LOG\"\necho 7");
        let router = junk_app(&mail_config(&fixture), &fake);
        let ((first, _), (second, _)) = tokio::join!(
            mark(&router, mail_fixture::MULTIPART_ALL_MAIL, false),
            mark(&router, mail_fixture::PLAIN, false),
        );
        assert_eq!(first, StatusCode::OK);
        assert_eq!(second, StatusCode::OK);
        assert_eq!(fake.calls(), ["start", "end", "start", "end"]);
    }

    const KID: &str = "key-1";
    const ISSUER: &str = "https://auth.example.com";
    const AUDIENCE: &str = "https://eventkit-bridge";
    const CLIENT: &str = "agent";
    const ALL_SCOPES: [&str; 6] = [
        "bridge:calendar.read",
        "bridge:calendar.write",
        "bridge:reminders.read",
        "bridge:reminders.write",
        "bridge:mail.read",
        "bridge:mail.junk",
    ];

    fn auth_table() -> AuthConfig {
        AuthConfig {
            issuer: ISSUER.to_owned(),
            audience: AUDIENCE.to_owned(),
            jwks_url: url::Url::parse("https://auth.example.com/jwks.json").unwrap(),
            scope_prefix: "bridge:".to_owned(),
        }
    }

    fn now() -> i64 {
        i64::try_from(jsonwebtoken::get_current_timestamp()).unwrap()
    }

    fn token_claims(scopes: &[&str]) -> Value {
        let now = now();
        json!({
            "iss": ISSUER,
            "sub": "subject-7",
            "client_id": CLIENT,
            "aud": [AUDIENCE],
            "exp": now + 900,
            "iat": now,
            "nbf": now,
            "jti": "jti-value",
            "scp": scopes,
        })
    }

    fn token(scopes: &[&str]) -> String {
        test_keys::mint(&token_claims(scopes), KID)
    }

    fn expired_token(scopes: &[&str]) -> String {
        let mut claims = token_claims(scopes);
        claims["exp"] = json!(now() - 3600);
        test_keys::mint(&claims, KID)
    }

    fn all_scopes_but(name: &str) -> Vec<&'static str> {
        let mut scopes = Vec::new();
        for scope in ALL_SCOPES {
            if scope != name {
                scopes.push(scope);
            }
        }
        scopes
    }

    struct AuthApp {
        _dir: tempfile::TempDir,
        _fixture: MailFixture,
        _osascript: Fake,
        router: Router,
    }

    fn auth_app(calendars: &Fake, kids: &[&str]) -> AuthApp {
        let dir = tempfile::tempdir().unwrap();
        let fixture = MailFixture::standard();
        let osascript = osascript_fake();
        let mut config = without_exclusions(&fixture);
        let table = auth_table();
        let authenticator = Authenticator::new(&table, test_source::preloaded(dir.path(), kids));
        config.auth = Some(table);
        let mut runners = runners(calendars.runner());
        runners.mail = script::Runner::new(osascript.program().to_owned(), Duration::from_secs(10));
        let router = router(Arc::new(App::new(
            &config,
            runners,
            Some(Arc::new(authenticator)),
        )));
        AuthApp {
            _dir: dir,
            _fixture: fixture,
            _osascript: osascript,
            router,
        }
    }

    fn with_token(mut request: Request<Body>, token: &str) -> Request<Body> {
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        request
    }

    struct Answer {
        status: StatusCode,
        challenge: Option<String>,
        body: Value,
    }

    async fn exchange(router: &Router, request: Request<Body>) -> Answer {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let challenge = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .map(|value| value.to_str().unwrap().to_owned());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = match bytes.is_empty() {
            true => Value::Null,
            false => serde_json::from_slice(&bytes).unwrap(),
        };
        Answer {
            status,
            challenge,
            body,
        }
    }

    fn empty(method: Method, uri: &str) -> Request<Body> {
        build_request(method, uri, Body::empty())
    }

    const INVALID_CHALLENGE: &str = r#"Bearer error="invalid_token""#;

    struct ScopedCase {
        scope: &'static str,
        method: Method,
        uri: String,
        body: Option<Value>,
        status: StatusCode,
    }

    fn scoped_cases() -> Vec<ScopedCase> {
        vec![
            ScopedCase {
                scope: "bridge:calendar.read",
                method: Method::GET,
                uri: EVENT_PATH.to_owned(),
                body: None,
                status: StatusCode::OK,
            },
            ScopedCase {
                scope: "bridge:calendar.write",
                method: Method::POST,
                uri: "/v1/events".to_owned(),
                body: Some(create_body()),
                status: StatusCode::CREATED,
            },
            ScopedCase {
                scope: "bridge:reminders.read",
                method: Method::GET,
                uri: "/v1/places".to_owned(),
                body: None,
                status: StatusCode::OK,
            },
            ScopedCase {
                scope: "bridge:reminders.write",
                method: Method::DELETE,
                uri: "/v1/reminders/42".to_owned(),
                body: None,
                status: StatusCode::BAD_REQUEST,
            },
            ScopedCase {
                scope: "bridge:mail.read",
                method: Method::GET,
                uri: "/v1/mail/accounts".to_owned(),
                body: None,
                status: StatusCode::OK,
            },
            ScopedCase {
                scope: "bridge:mail.junk",
                method: Method::PATCH,
                uri: format!("/v1/mail/messages/{}", mail_fixture::MULTIPART),
                body: Some(json!({ "junk": true })),
                status: StatusCode::OK,
            },
        ]
    }

    fn scoped_request(case: &ScopedCase) -> Request<Body> {
        let ScopedCase {
            scope: _,
            method,
            uri,
            body,
            status: _,
        } = case;
        match body {
            Some(body) => json_request(method.clone(), uri, body),
            None => empty(method.clone(), uri),
        }
    }

    #[tokio::test]
    async fn each_scope_reaches_its_route_and_only_its_scope_does() {
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);
        for case in scoped_cases() {
            let answer = exchange(
                &app.router,
                with_token(scoped_request(&case), &token(&[case.scope])),
            )
            .await;
            assert_eq!(
                answer.status, case.status,
                "{}: {}",
                case.scope, answer.body
            );
            assert_eq!(answer.challenge, None, "{}", case.scope);

            let answer = exchange(
                &app.router,
                with_token(scoped_request(&case), &token(&all_scopes_but(case.scope))),
            )
            .await;
            assert_eq!(answer.status, StatusCode::FORBIDDEN, "{}", case.scope);
            assert_eq!(
                answer.challenge,
                Some(format!(
                    r#"Bearer error="insufficient_scope", scope="{}""#,
                    case.scope
                ))
            );
            assert_eq!(
                answer.body,
                error(&format!("insufficient scope: {} needed", case.scope))
            );
        }
    }

    #[tokio::test]
    async fn read_token_on_a_write_route_is_forbidden_before_ekctl() {
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);
        let request = json_request(Method::POST, "/v1/events", &create_body());

        let answer = exchange(
            &app.router,
            with_token(request, &token(&["bridge:calendar.read"])),
        )
        .await;

        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        assert_eq!(
            answer.challenge.as_deref(),
            Some(r#"Bearer error="insufficient_scope", scope="bridge:calendar.write""#)
        );
        assert_eq!(
            answer.body,
            error("insufficient scope: bridge:calendar.write needed")
        );
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn head_needs_the_read_scope() {
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);

        let answer = exchange(
            &app.router,
            with_token(empty(Method::HEAD, EVENT_PATH), &token(&[])),
        )
        .await;

        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        assert_eq!(
            answer.challenge.as_deref(),
            Some(r#"Bearer error="insufficient_scope", scope="bridge:calendar.read""#)
        );
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn missing_token_is_unauthorized_on_every_route_group() {
        let (captured, _guard) = capture();
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);
        for case in scoped_cases() {
            let answer = exchange(&app.router, scoped_request(&case)).await;

            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{}", case.scope);
            assert_eq!(
                answer.challenge.as_deref(),
                Some("Bearer"),
                "{}",
                case.scope
            );
            assert_eq!(answer.body, error("missing bearer token"), "{}", case.scope);
        }
        assert!(fake.calls().is_empty());
        let log = captured.text();
        assert!(log.contains("token refused"), "{log}");
        assert!(!log.contains("anonymous"), "{log}");
    }

    #[tokio::test]
    async fn invalid_tokens_are_unauthorized() {
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);
        let mut no_audience = token_claims(&ALL_SCOPES);
        no_audience.as_object_mut().unwrap().remove("aud");
        let tokens = [
            expired_token(&ALL_SCOPES),
            test_keys::mint(&no_audience, KID),
            test_keys::OTHER_KEY.sign(&test_keys::header(KID), &token_claims(&ALL_SCOPES)),
            test_keys::mint(&token_claims(&ALL_SCOPES), "key-unknown"),
            "not-a-jwt".to_owned(),
        ];
        for token in tokens {
            let answer = exchange(
                &app.router,
                with_token(empty(Method::GET, EVENT_PATH), &token),
            )
            .await;

            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{token}");
            assert_eq!(answer.challenge.as_deref(), Some(INVALID_CHALLENGE));
            assert_eq!(answer.body, error("invalid bearer token"));
        }
        let mut request = empty(Method::GET, EVENT_PATH);
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        let answer = exchange(&app.router, request).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(answer.challenge.as_deref(), Some(INVALID_CHALLENGE));
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn healthz_needs_no_token_and_reports_the_keys() {
        let fake = Fake::printing("list_calendars.json");
        let app = auth_app(&fake, &[KID, "key-2"]);
        for request in [
            empty(Method::GET, "/healthz"),
            with_token(empty(Method::GET, "/healthz"), "not-a-jwt"),
        ] {
            let answer = exchange(&app.router, request).await;

            assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
            assert_eq!(answer.challenge, None);
            assert_eq!(answer.body["auth"]["jwks_keys"], json!(2));
            assert!(answer.body["auth"]["jwks_age_s"].as_u64().unwrap() <= 1);
        }
    }

    #[tokio::test]
    async fn healthz_without_keys_is_degraded() {
        let fake = Fake::printing("list_calendars.json");
        let app = auth_app(&fake, &[]);

        let answer = exchange(&app.router, empty(Method::GET, "/healthz")).await;

        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            answer.body,
            json!({"status": "degraded", "reason": "auth jwks unavailable"})
        );
    }

    #[tokio::test]
    async fn healthz_without_auth_has_no_auth_field() {
        let fake = Fake::printing("list_calendars.json");
        let router = app(&configured(), fake.runner());

        let (status, body) = get(&router, "/healthz").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.get("auth"), None);
    }

    #[tokio::test]
    async fn unknown_paths_and_methods_need_a_token_but_no_scope() {
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);
        let scopeless = token(&[]);

        let answer = exchange(
            &app.router,
            with_token(empty(Method::GET, "/v1/tasks"), &scopeless),
        )
        .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND);
        assert_eq!(answer.body, error("not found"));

        let answer = exchange(&app.router, empty(Method::GET, "/v1/tasks")).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(answer.challenge.as_deref(), Some("Bearer"));

        let answer = exchange(
            &app.router,
            with_token(empty(Method::DELETE, "/v1/calendars"), &scopeless),
        )
        .await;
        assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED);

        let answer = exchange(&app.router, empty(Method::DELETE, "/v1/calendars")).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn host_check_runs_before_the_token() {
        let (captured, _guard) = capture();
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);
        let request = Request::builder()
            .uri("/v1/calendars")
            .header("host", "rebound.example.com")
            .body(Body::empty())
            .unwrap();

        let answer = exchange(&app.router, request).await;

        assert_eq!(answer.status, StatusCode::MISDIRECTED_REQUEST);
        assert_eq!(answer.challenge, None);
        assert!(!captured.text().contains("token refused"));
    }

    #[tokio::test]
    async fn body_limit_applies_after_a_valid_token() {
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);
        let request = build_request(
            Method::POST,
            "/v1/events",
            Body::from(vec![b' '; BODY_LIMIT + 1]),
        );

        let answer = exchange(
            &app.router,
            with_token(request, &token(&["bridge:calendar.write"])),
        )
        .await;

        assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn every_route_but_health_has_a_scope() {
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);
        let scopeless = token(&[]);
        let all = token(&ALL_SCOPES);
        let methods = [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
            Method::TRACE,
        ];
        for (path, _) in routes() {
            let uri = path.replace("{id}", "probe");
            for method in &methods {
                let scope = scope_for(method, path);
                let answer = exchange(
                    &app.router,
                    with_token(empty(method.clone(), &uri), &scopeless),
                )
                .await;
                if path == EXEMPT_ROUTE {
                    assert_eq!(scope, None);
                    assert_ne!(answer.status, StatusCode::UNAUTHORIZED, "{method} {path}");
                    assert_ne!(answer.status, StatusCode::FORBIDDEN, "{method} {path}");
                    continue;
                }
                let Some(scope) = scope else {
                    assert_eq!(
                        answer.status,
                        StatusCode::METHOD_NOT_ALLOWED,
                        "{method} {path} has a handler but no row in the scope table"
                    );
                    continue;
                };
                assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {path}");
                let answer =
                    exchange(&app.router, with_token(empty(method.clone(), &uri), &all)).await;
                assert_ne!(
                    answer.status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path} has the scope {scope} but no handler"
                );
                assert_eq!(answer.challenge, None, "{method} {path}");
            }
        }
    }

    #[tokio::test]
    async fn auth_request_log_carries_the_client_but_no_token() {
        let (captured, _guard) = capture();
        let fake = write_fake();
        let app = auth_app(&fake, &[KID]);
        let valid = token(&["bridge:calendar.read"]);
        let expired = expired_token(&["bridge:calendar.read"]);
        let wrong_scope = token(&["bridge:mail.read"]);
        let tokens = [&valid, &expired, &wrong_scope];
        for token in tokens {
            exchange(
                &app.router,
                with_token(empty(Method::GET, EVENT_PATH), token),
            )
            .await;
        }
        exchange(&app.router, empty(Method::GET, EVENT_PATH)).await;

        let log = captured.text();
        for token in tokens {
            assert!(!log.contains(token.as_str()), "token leaked into:\n{log}");
            for segment in token.split('.') {
                assert!(!log.contains(segment), "{segment} leaked into:\n{log}");
            }
        }
        for secret in [AUDIENCE, ISSUER, "scp", "subject-7", "jti-value", "Bearer"] {
            assert!(!log.contains(secret), "{secret} leaked into:\n{log}");
        }
        let mut requests = Vec::new();
        for line in log.lines() {
            if line.contains(" request method=") {
                requests.push(line);
            }
        }
        let client = format!(" client={CLIENT}");
        assert_eq!(requests.len(), 4, "{log}");
        assert!(requests[0].contains(" status=200 "), "{log}");
        assert!(requests[0].ends_with(&client), "{log}");
        assert!(requests[1].contains(" status=401 "), "{log}");
        assert!(!requests[1].contains("client="), "{log}");
        assert!(requests[2].contains(" status=403 "), "{log}");
        assert!(requests[2].ends_with(&client), "{log}");
        assert!(requests[3].contains(" status=401 "), "{log}");
        assert!(!requests[3].contains("client="), "{log}");
        assert!(log.contains("token refused kind=invalid"), "{log}");
        assert!(
            log.contains(r#"token refused kind="insufficient scope""#),
            "{log}"
        );
        assert!(log.contains("token refused kind=missing"), "{log}");
        assert!(log.contains("status=403"), "{log}");
        assert!(log.contains("status=401"), "{log}");
    }

    #[tokio::test(start_paused = true)]
    async fn jwks_fetch_logs_only_the_failure_kind() {
        let (captured, _guard) = capture();
        let dir = tempfile::tempdir().unwrap();
        let source = test_source::ScriptedSource::new(vec![
            Ok(b"<html>jwks-body-marker</html>".to_vec()),
            Err(crate::auth::FetchError::Status(503)),
            Ok(test_source::body(&["kid-in-the-body"])),
        ]);
        let jwks = Arc::new(crate::auth::Jwks::new(
            Arc::new(source),
            dir.path().join("jwks-cache.json"),
        ));

        for _ in 0..3 {
            jwks.refetch_unknown_key().await;
            tokio::time::advance(Duration::from_secs(60)).await;
        }

        assert_eq!(jwks.key_count(), 1);
        let log = captured.text();
        assert!(
            log.contains("cannot fetch the jwks for an unknown key error=not a jwk set"),
            "{log}"
        );
        assert!(
            log.contains("cannot fetch the jwks for an unknown key error=status 503"),
            "{log}"
        );
        assert!(
            log.contains(r#"jwks loaded keys=1 from="provider""#),
            "{log}"
        );
        let jwk = test_keys::KEY.jwk_json("kid-in-the-body");
        let modulus = jwk["n"].as_str().unwrap();
        for secret in [
            "jwks-body-marker",
            "<html>",
            "kid-in-the-body",
            "\"keys\"",
            modulus,
        ] {
            assert!(!log.contains(secret), "{secret} leaked into:\n{log}");
        }
    }

    #[tokio::test]
    async fn request_log_without_auth_has_no_client() {
        let (captured, _guard) = capture();
        let fake = write_fake();
        let router = app(&configured(), fake.runner());

        get(&router, EVENT_PATH).await;

        let log = captured.text();
        assert!(log.contains("status=200"), "{log}");
        assert!(!log.contains("client="), "{log}");
    }
}
