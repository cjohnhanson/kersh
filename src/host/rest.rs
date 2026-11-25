//! REST on `host.sock`: the routes, their handlers, and the `OpenAPI`
//! document generated from the same types the handlers return.
//!
//! Each handler calls the host method or [`Client`] read the matching
//! command calls, so the answer is the command's `--json` output: one
//! object, or an array of the objects the command prints one per line.
//! The socket is the only listener, a file in a 0700 directory; nothing
//! here authenticates, because only this user's processes reach it.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::broadcast;
use utoipa::{IntoParams, OpenApi};

use super::Host;
use crate::client::Client;
use crate::events::Event;
use crate::proto::{self, Launch, ResumeBody, SendBody, VersionInfo};
use crate::runtime::RuntimeEntry;
use crate::session::{HarnessBuild, Record, Screen, SessionError, StartRequest, State as Phase};

/// The `OpenAPI` document of every route.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "kersh",
        description = "The kersh host on its Unix socket, `host.sock`. Bodies are the JSON \
                       the matching command prints with `--json`; a list is an array of the \
                       objects the command prints one per line. Requests and responses carry \
                       `kersh-schema` and `kersh-version` headers; a request of another \
                       schema is refused."
    ),
    paths(
        version,
        list_sessions,
        start_session,
        launch,
        get_session,
        rm_session,
        stop,
        resume,
        restart,
        send,
        events,
        screen,
        held_return,
        capture,
        changes,
        restart_mux,
        harnesses,
        runtimes,
        runtime,
    ),
    components(schemas(SessionError, Record, StartRequest, Launch, Event, Screen)),
    modifiers(&SchemaRefusal)
)]
pub struct ApiDoc;

/// The 400 every route can answer: the schema check runs before each
/// handler.
struct SchemaRefusal;

impl utoipa::Modify for SchemaRefusal {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::{Content, Ref, ResponseBuilder};
        let refused = ResponseBuilder::new()
            .description("A bad request, or a request of another `kersh-schema`.")
            .content(
                "application/json",
                Content::new(Some(Ref::from_schema_name("SessionError"))),
            )
            .build();
        for item in openapi.paths.paths.values_mut() {
            for operation in [
                &mut item.get,
                &mut item.put,
                &mut item.post,
                &mut item.delete,
                &mut item.patch,
            ]
            .into_iter()
            .flatten()
            {
                operation
                    .responses
                    .responses
                    .insert("400".to_owned(), refused.clone().into());
            }
        }
    }
}

/// The document as JSON, as `GET /openapi.json` serves it.
#[must_use]
pub fn openapi_json() -> String {
    ApiDoc::openapi().to_json().unwrap_or_default()
}

type Shared = Arc<Host>;

/// A failed call, answered with the error's status and JSON.
struct Failure(StatusCode, SessionError);

impl From<SessionError> for Failure {
    fn from(error: SessionError) -> Self {
        let status =
            StatusCode::from_u16(proto::status_of(&error)).unwrap_or(StatusCode::BAD_REQUEST);
        Self(status, error)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        json_response(self.0, &self.1)
    }
}

type Answer = Result<Response, Failure>;

fn json_response(status: StatusCode, value: &impl serde::Serialize) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

// Every handler answers through `Answer`, so these helpers return it.
#[allow(clippy::unnecessary_wraps)]
fn ok(value: &impl serde::Serialize) -> Answer {
    Ok(json_response(StatusCode::OK, value))
}

#[allow(clippy::unnecessary_wraps)]
fn no_content() -> Answer {
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A JSON body, or a usage error naming what is wrong with it. An empty
/// body is the type's default.
fn body<T: DeserializeOwned + Default>(bytes: &Bytes) -> Result<T, Failure> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(bytes)
        .map_err(|error| SessionError::usage(format!("not a valid body: {error}")).into())
}

fn required<T: DeserializeOwned>(bytes: &Bytes) -> Result<T, Failure> {
    serde_json::from_slice(bytes)
        .map_err(|error| SessionError::usage(format!("not a valid body: {error}")).into())
}

fn query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, Failure> {
    query
        .map(|Query(value)| value)
        .map_err(|error| SessionError::usage(error.body_text()).into())
}

/// Run file and session-server reads off the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, SessionError> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| SessionError::failed(format!("the call did not finish: {error}")))?
        .map_err(Failure::from)
}

/// Run a write to its end in a task of its own. The connection's future
/// is dropped when its client hangs up, and a write must never stop half
/// done: an accepted RET ends the asking wrapper's pane, and so its
/// connection, before the relaunch it asked for.
async fn write<T: Send + 'static>(
    work: impl Future<Output = Result<T, SessionError>> + Send + 'static,
) -> Result<T, Failure> {
    tokio::spawn(work)
        .await
        .map_err(|error| SessionError::failed(format!("the call did not finish: {error}")))?
        .map_err(Failure::from)
}

impl Host {
    /// A client over this host's state, for the reads the commands make
    /// without a host.
    fn reader(&self) -> Client {
        Client::discover(&self.paths.state).with_paths(self.paths.clone())
    }
}

/// Every route, with the schema check on each request and the schema
/// headers on each response.
pub(super) fn router(host: Shared) -> Router {
    Router::new()
        .route("/openapi.json", get(openapi))
        .route("/v1/version", get(version))
        .route("/v1/sessions", get(list_sessions).post(start_session))
        .route("/v1/launches", post(launch))
        .route("/v1/sessions/{id}", get(get_session).delete(rm_session))
        .route("/v1/sessions/{id}/stop", post(stop))
        .route("/v1/sessions/{id}/resume", post(resume))
        .route("/v1/sessions/{id}/restart", post(restart))
        .route("/v1/sessions/{id}/send", post(send))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/sessions/{id}/screen", get(screen))
        .route("/v1/sessions/{id}/held-return", post(held_return))
        .route("/v1/sessions/{id}/capture", post(capture))
        .route("/v1/changes", get(changes))
        .route("/v1/mux/restart", post(restart_mux))
        .route("/v1/harnesses", get(harnesses))
        .route("/v1/runtimes", get(runtimes))
        .route("/v1/runtimes/{name}", get(runtime))
        .fallback(no_route)
        .layer(axum::middleware::from_fn(schema))
        .with_state(host)
}

/// Refuse a request of another protocol schema, and name this build's
/// schema and version on every response.
async fn schema(request: Request, next: Next) -> Response {
    let checked = {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        proto::check_schema(
            "the request",
            header(proto::SCHEMA_HEADER).as_deref(),
            header(proto::VERSION_HEADER).as_deref(),
        )
    };
    let mut response = match checked {
        Ok(()) => next.run(request).await,
        Err(error) => Failure::from(error).into_response(),
    };
    let headers = response.headers_mut();
    headers.insert(
        proto::SCHEMA_HEADER,
        HeaderValue::from(proto::PROTOCOL_SCHEMA),
    );
    headers.insert(
        proto::VERSION_HEADER,
        HeaderValue::from_static(proto::VERSION),
    );
    response
}

async fn no_route(request: Request) -> Response {
    let error = SessionError::usage(format!(
        "no route {} {}; GET /openapi.json lists them",
        request.method(),
        request.uri().path()
    ));
    json_response(StatusCode::NOT_FOUND, &error)
}

async fn openapi() -> Response {
    ([(header::CONTENT_TYPE, "application/json")], openapi_json()).into_response()
}

/// This build's version and protocol schema.
#[utoipa::path(get, path = "/v1/version", responses((status = 200, body = VersionInfo)))]
async fn version() -> Answer {
    ok(&VersionInfo {
        version: proto::VERSION.to_owned(),
        schema: proto::PROTOCOL_SCHEMA,
    })
}

/// `kersh ps`'s filters.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct PsQuery {
    /// Only sessions in this state.
    state: Option<Phase>,
    /// Only these sessions, separated by commas.
    ids: Option<String>,
}

/// Sessions, from the records on disk: `kersh ps --json`.
#[utoipa::path(
    get,
    path = "/v1/sessions",
    params(PsQuery),
    responses(
        (status = 200, body = Vec<Record>),
        (status = 404, body = SessionError, description = "a named session does not exist")
    )
)]
async fn list_sessions(
    State(host): State<Shared>,
    filter: Result<Query<PsQuery>, QueryRejection>,
) -> Answer {
    let filter = query(filter)?;
    let ids: Vec<String> = filter
        .ids
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    let reader = host.reader();
    let records = blocking(move || reader.ps(&ids, filter.state)).await?;
    ok(&records)
}

/// Start a session: `kersh start --json`. The agent file, its skill, and
/// a relative `instructions_file` resolve in `cwd`, which is required.
#[utoipa::path(
    post,
    path = "/v1/sessions",
    request_body = StartRequest,
    responses(
        (status = 201, body = Record),
        (status = 400, body = SessionError),
        (status = 409, body = SessionError, description = "the group is at its limit"),
        (status = 500, body = SessionError, description = "the session did not start")
    )
)]
async fn start_session(State(host): State<Shared>, bytes: Bytes) -> Answer {
    let mut request: StartRequest = required(&bytes)?;
    let Some(cwd) = request.cwd.clone().filter(|cwd| cwd.is_absolute()) else {
        return Err(SessionError::usage(
            "give `cwd`, the session's working directory, as an absolute path",
        )
        .into());
    };
    if let Some(file) = request.instructions_file.take() {
        request.instructions_file = Some(if file.is_absolute() {
            file
        } else {
            cwd.join(file)
        });
    }
    let paths = host.paths.clone();
    let launch = blocking(move || {
        Client::discover(&cwd)
            .with_paths(paths)
            .resolve_start(&request)
    })
    .await?;
    let record = write(async move { host.start(launch).await }).await?;
    Ok(json_response(StatusCode::CREATED, &record))
}

/// Start a session a linked client already resolved, as
/// `kersh::Client::start` does.
#[utoipa::path(
    post,
    path = "/v1/launches",
    request_body = Launch,
    responses(
        (status = 201, body = Record),
        (status = 400, body = SessionError),
        (status = 409, body = SessionError, description = "the group is at its limit"),
        (status = 500, body = SessionError, description = "the session did not start")
    )
)]
async fn launch(State(host): State<Shared>, bytes: Bytes) -> Answer {
    let launch: Launch = required(&bytes)?;
    let record = write(async move { host.start(launch).await }).await?;
    Ok(json_response(StatusCode::CREATED, &record))
}

/// One session's record: the line `kersh ps <id> --json` prints.
#[utoipa::path(
    get,
    path = "/v1/sessions/{id}",
    params(("id" = String, Path, description = "The session.")),
    responses((status = 200, body = Record), (status = 404, body = SessionError))
)]
async fn get_session(State(host): State<Shared>, Path(id): Path<String>) -> Answer {
    let reader = host.reader();
    let mut records = blocking(move || reader.ps(&[id], None)).await?;
    match records.pop() {
        Some(record) => ok(&record),
        None => Err(SessionError::failed("the record vanished").into()),
    }
}

/// Delete an ended session's record, events, and temp dir: `kersh rm`.
#[utoipa::path(
    delete,
    path = "/v1/sessions/{id}",
    params(("id" = String, Path, description = "The session.")),
    responses((status = 204), (status = 404, body = SessionError), (status = 500, body = SessionError))
)]
async fn rm_session(State(host): State<Shared>, Path(id): Path<String>) -> Answer {
    write(async move { host.rm(&id).await }).await?;
    no_content()
}

/// End a session and close its pane: `kersh stop --json`.
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/stop",
    params(("id" = String, Path, description = "The session.")),
    responses((status = 200, body = Record), (status = 404, body = SessionError))
)]
async fn stop(State(host): State<Shared>, Path(id): Path<String>) -> Answer {
    ok(&write(async move { host.stop(&id).await }).await?)
}

/// Continue a held or ended session's conversation: `kersh resume --json`.
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/resume",
    params(("id" = String, Path, description = "The session.")),
    request_body = ResumeBody,
    responses(
        (status = 200, body = Record),
        (status = 400, body = SessionError),
        (status = 404, body = SessionError),
        (status = 409, body = SessionError, description = "the group is at its limit")
    )
)]
async fn resume(State(host): State<Shared>, Path(id): Path<String>, bytes: Bytes) -> Answer {
    let ResumeBody { prompt } = body(&bytes)?;
    ok(&write(async move { host.resume(&id, prompt).await }).await?)
}

/// Restart an interactive session on its conversation: `kersh restart
/// --json`.
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/restart",
    params(("id" = String, Path, description = "The session.")),
    responses((status = 200, body = Record), (status = 400, body = SessionError), (status = 404, body = SessionError))
)]
async fn restart(State(host): State<Shared>, Path(id): Path<String>) -> Answer {
    ok(&write(async move { host.restart(&id).await }).await?)
}

/// Type text into a running interactive session, then Enter: `kersh
/// send`.
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/send",
    params(("id" = String, Path, description = "The session.")),
    request_body = SendBody,
    responses((status = 204), (status = 404, body = SessionError), (status = 500, body = SessionError))
)]
async fn send(State(host): State<Shared>, Path(id): Path<String>, bytes: Bytes) -> Answer {
    let SendBody { text } = required(&bytes)?;
    write(async move { host.send(&id, &text).await }).await?;
    no_content()
}

/// `kersh events`'s filter.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct EventsQuery {
    /// Only events after this sequence number.
    since: Option<u64>,
}

/// A headless session's events: `kersh events`.
#[utoipa::path(
    get,
    path = "/v1/sessions/{id}/events",
    params(("id" = String, Path, description = "The session."), EventsQuery),
    responses((status = 200, body = Vec<Event>), (status = 404, body = SessionError))
)]
async fn events(
    State(host): State<Shared>,
    Path(id): Path<String>,
    filter: Result<Query<EventsQuery>, QueryRejection>,
) -> Answer {
    let since = query(filter)?.since.unwrap_or(0);
    let reader = host.reader();
    ok(&blocking(move || reader.events(&id, since)).await?)
}

/// A session's screen: `kersh screen --json`.
#[utoipa::path(
    get,
    path = "/v1/sessions/{id}/screen",
    params(("id" = String, Path, description = "The session.")),
    responses((status = 200, body = Screen), (status = 404, body = SessionError), (status = 500, body = SessionError))
)]
async fn screen(State(host): State<Shared>, Path(id): Path<String>) -> Answer {
    let reader = host.reader();
    ok(&blocking(move || reader.screen(&id)).await?)
}

/// RET in a held pane, from the pane's wrapper.
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/held-return",
    params(("id" = String, Path, description = "The session.")),
    responses((status = 204), (status = 400, body = SessionError), (status = 409, body = SessionError))
)]
async fn held_return(State(host): State<Shared>, Path(id): Path<String>) -> Answer {
    write(async move { host.held_return(&id).await }).await?;
    no_content()
}

/// Capture a session's screen now, as after a detach.
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/capture",
    params(("id" = String, Path, description = "The session.")),
    responses((status = 204), (status = 404, body = SessionError))
)]
async fn capture(State(host): State<Shared>, Path(id): Path<String>) -> Answer {
    write(async move {
        let record = host.load(&id)?;
        let mux = host.mux().await?;
        host.capture_one(&mux, &record).await;
        Ok(())
    })
    .await?;
    no_content()
}

/// The records a change stream sends, read off the async workers. A
/// record that does not read is logged and skipped, as on the host's
/// background paths.
async fn readable_records(host: Shared) -> Vec<Record> {
    tokio::task::spawn_blocking(move || host.background_records())
        .await
        .unwrap_or_else(|error| {
            eprintln!("kersh host: /v1/changes could not read the records: {error}");
            Vec::new()
        })
}

/// Every record, then each change, one JSON object per line, until the
/// client hangs up: `kersh ps --follow --json`.
#[utoipa::path(
    get,
    path = "/v1/changes",
    responses((status = 200, content_type = "application/x-ndjson", body = Record))
)]
async fn changes(State(host): State<Shared>) -> Response {
    // Subscribe before reading the records, so no change falls between.
    let receiver = host.changes.subscribe();
    let first = readable_records(Arc::clone(&host)).await;
    let stream = futures_util::stream::unfold(
        (first.into_iter(), receiver, host),
        |(mut pending, mut receiver, host)| async move {
            loop {
                if let Some(record) = pending.next() {
                    let mut line = serde_json::to_vec(&record).unwrap_or_default();
                    line.push(b'\n');
                    return Some((Ok::<_, Infallible>(line), (pending, receiver, host)));
                }
                match receiver.recv().await {
                    Ok(record) => pending = vec![record].into_iter(),
                    // Too far behind: every record again, as at the start.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        pending = readable_records(Arc::clone(&host)).await.into_iter();
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );
    (
        [(header::CONTENT_TYPE, proto::NDJSON)],
        Body::from_stream(stream),
    )
        .into_response()
}

/// Restart the session server and restore every session: `kersh restart
/// --mux --json`.
#[utoipa::path(
    post,
    path = "/v1/mux/restart",
    responses((status = 200, body = Vec<Record>), (status = 500, body = SessionError))
)]
async fn restart_mux(State(host): State<Shared>) -> Answer {
    ok(&write(async move { host.restart_mux().await }).await?)
}

/// The build each harness would run: `kersh harness list --json`.
#[utoipa::path(get, path = "/v1/harnesses", responses((status = 200, body = Vec<HarnessBuild>)))]
async fn harnesses(State(host): State<Shared>) -> Answer {
    ok(&crate::harness::list(&host.login_env))
}

/// Every runtime: `kersh runtime list --json`.
#[utoipa::path(
    get,
    path = "/v1/runtimes",
    responses((status = 200, body = Vec<RuntimeEntry>), (status = 500, body = SessionError))
)]
async fn runtimes(State(host): State<Shared>) -> Answer {
    let reader = host.reader();
    ok(&blocking(move || reader.runtime_list()).await?)
}

/// One runtime: `kersh runtime show --json`.
#[utoipa::path(
    get,
    path = "/v1/runtimes/{name}",
    params(("name" = String, Path, description = "The runtime.")),
    responses((status = 200, body = RuntimeEntry), (status = 400, body = SessionError))
)]
async fn runtime(State(host): State<Shared>, Path(name): Path<String>) -> Answer {
    let reader = host.reader();
    ok(&blocking(move || reader.runtime_show(&name)).await?)
}
