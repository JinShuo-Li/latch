//! Authenticated loopback transport; no workspace directory is served.
use super::{ApiError, Host, actor::Action};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{delete, get, post},
};
use futures::stream;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{convert::Infallible, time::Duration};
use uuid::Uuid;

pub fn router(host: Host) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/{module}", get(module))
        .route("/styles.css", get(style))
        .route("/mark.svg", get(mark))
        .route("/api/auth", post(auth))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/events", get(events))
        .route("/api/sessions", get(sessions).post(create))
        .route("/api/sessions/{id}/activate", post(activate))
        .route("/api/sessions/{id}/commands", post(command))
        .route(
            "/api/sessions/{id}/attachments",
            post(upload).layer(DefaultBodyLimit::max(
                latch_kernel::media::MAX_IMAGE_BYTES as usize,
            )),
        )
        .route("/api/sessions/{id}/media/{artifact}", get(media))
        .route("/api/sessions/{id}/attachments/{artifact}", delete(detach))
        .layer(DefaultBodyLimit::max(512 * 1024))
        .layer(middleware::from_fn_with_state(host.clone(), guard))
        .with_state(host)
}

fn cookie_name(host: &Host) -> String {
    format!("latch_web_{}", host.0.instance.simple())
}
fn equal_secret(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |a, (l, r)| a | (l ^ r))
            == 0
}
fn authorized(host: &Host, headers: &HeaderMap) -> bool {
    let expected = cookie_name(host);
    headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|cookies| {
            cookies
                .split(';')
                .filter_map(|c| c.trim().split_once('='))
                .any(|(name, value)| name == expected && equal_secret(value, &host.0.cookie))
        })
}
async fn guard(State(host): State<Host>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let authority = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let valid_host = authority
        .and_then(|h| h.parse::<axum::http::uri::Authority>().ok())
        .is_some_and(|a| matches!(a.host(), "localhost" | "127.0.0.1"));
    if !valid_host || headers.get_all(header::HOST).iter().count() != 1 {
        return ApiError(
            StatusCode::FORBIDDEN,
            "use a localhost or 127.0.0.1 address".into(),
        )
        .into_response();
    }
    let origin = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok());
    let expected = format!("http://{}", authority.unwrap_or_default());
    let mutation = !matches!(
        request.method(),
        &axum::http::Method::GET | &axum::http::Method::HEAD
    );
    if origin.is_some_and(|o| o != expected) || (mutation && origin != Some(expected.as_str())) {
        return ApiError(StatusCode::FORBIDDEN, "same-origin request required".into())
            .into_response();
    }
    if headers
        .get("sec-fetch-site")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| !matches!(s, "same-origin" | "none"))
    {
        return ApiError(
            StatusCode::FORBIDDEN,
            "cross-origin access is not allowed".into(),
        )
        .into_response();
    }
    if request.uri().path().starts_with("/api/")
        && request.uri().path() != "/api/auth"
        && !authorized(&host, headers)
    {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "enter the access token printed by Latch".into(),
        )
        .into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::CONTENT_SECURITY_POLICY,"default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' blob:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
    response
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../../../../web/app/index.html"),
    )
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../../../../web/app/app.js"),
    )
}
async fn module(Path(name): Path<String>) -> Result<Response, ApiError> {
    let source = match name.as_str() {
        "transport.js" => include_str!("../../../../web/app/transport.js"),
        "state.js" => include_str!("../../../../web/app/state.js"),
        "view.js" => include_str!("../../../../web/app/view.js"),
        "icons.js" => include_str!("../../../../web/app/icons.js"),
        "settings.js" => include_str!("../../../../web/app/settings.js"),
        _ => return Err(ApiError(StatusCode::NOT_FOUND, "unknown asset".into())),
    };
    Ok((
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        source,
    )
        .into_response())
}
async fn detach(
    State(host): State<Host>,
    Path((session, artifact)): Path<(Uuid, String)>,
) -> Result<Json<Value>, ApiError> {
    host.request(Action::Detach { session, artifact })
        .await
        .map(Json)
}
async fn style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../../../web/app/styles.css"),
    )
}
async fn mark() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("../../../../web/app/mark.svg"),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Auth {
    token: String,
}
async fn auth(State(host): State<Host>, Json(body): Json<Auth>) -> Result<Response, ApiError> {
    if !equal_secret(&body.token, &host.0.token) {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid access token".into(),
        ));
    }
    Ok((
        [(
            header::SET_COOKIE,
            format!(
                "{}={}; HttpOnly; SameSite=Strict; Path=/",
                cookie_name(&host),
                host.0.cookie
            ),
        )],
        Json(json!({"authenticated":true})),
    )
        .into_response())
}
async fn bootstrap(State(host): State<Host>) -> Json<Value> {
    Json(host.snapshot().await)
}
async fn sessions(State(host): State<Host>) -> Result<Json<Value>, ApiError> {
    let sessions = host
        .database()
        .and_then(|s| s.list_sessions(Some(&host.0.workspace)))
        .map_err(ApiError::runtime)?;
    Ok(Json(
        json!({"sessions":sessions.into_iter().map(|s|json!({"id":s.id,"workspace":s.workspace,"created_at":s.created_at,"updated_at":s.updated_at,"mode":s.mode,"model":s.model,"completion":s.completion,"prompt_preview":s.prompt_preview,"event_count":s.event_count})).collect::<Vec<_>>()}),
    ))
}
async fn create(State(host): State<Host>) -> Result<Json<Value>, ApiError> {
    host.request(Action::Activate(None, Vec::new()))
        .await
        .map(Json)
}
async fn activate(State(host): State<Host>, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiError> {
    host.request(Action::Activate(Some(id), Vec::new()))
        .await
        .map(Json)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    command_id: Uuid,
    instance_id: Uuid,
    issued_at: i64,
    input: latch_ui::Input,
}
async fn command(
    State(host): State<Host>,
    Path(id): Path<Uuid>,
    Json(body): Json<Command>,
) -> Result<Json<Value>, ApiError> {
    let now = chrono::Utc::now().timestamp_millis();
    if body.instance_id != host.0.instance
        || body.issued_at > now + 1000
        || now.saturating_sub(body.issued_at) > 300_000
    {
        return Err(ApiError::conflict(
            "command expired or server restarted; reload before submitting",
        ));
    }
    host.request(Action::Command {
        session: id,
        command: body.command_id,
        input: body.input,
    })
    .await
    .map(Json)
}
async fn upload(
    State(host): State<Host>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let name = headers
        .get("x-image-name")
        .and_then(|h| h.to_str().ok())
        .filter(|n| n.len() <= 256)
        .map(str::to_owned);
    host.request(Action::Upload {
        session: id,
        bytes: bytes.to_vec(),
        name,
    })
    .await
    .map(Json)
}
async fn media(
    State(host): State<Host>,
    Path((id, artifact)): Path<(Uuid, String)>,
) -> Result<Response, ApiError> {
    let view = host.0.view.lock().await;
    if view.session_id != Some(id) {
        return Err(ApiError::conflict(
            "select this session before viewing its media",
        ));
    }
    let reference = view
        .media
        .get(&artifact)
        .cloned()
        .ok_or_else(|| ApiError::invalid("unknown session media"))?;
    drop(view);
    let root = host.artifacts(id);
    let mime = reference.mime_type.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        latch_kernel::media::read_image_bytes(&root, &reference)
    })
    .await
    .map_err(ApiError::runtime)?
    .map_err(ApiError::runtime)?;
    Ok(([(header::CONTENT_TYPE, mime)], bytes).into_response())
}
async fn events(
    State(host): State<Host>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before snapshotting. A reconnect always starts from an
    // authoritative projection; Last-Event-ID is never durable history.
    let receiver = host.0.changes.subscribe();
    let snapshot = host.snapshot().await;
    let initial = Some(snapshot);
    let stream = stream::unfold(
        (host, receiver, initial),
        |(host, mut receiver, mut initial)| async move {
            let event = if let Some(snapshot) = initial.take() {
                Event::default()
                    .event("snapshot")
                    .data(snapshot.to_string())
            } else {
                match tokio::select! { _ = host.0.shutdown.cancelled() => return None, result = receiver.recv() => result }
                {
                    Ok(sequence) => Event::default()
                        .event("changed")
                        .id(format!("{}:{sequence}", host.0.instance))
                        .data(
                            json!({"instance_id":host.0.instance,"sequence":sequence}).to_string(),
                        ),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Event::default()
                        .event("snapshot")
                        .data(host.snapshot().await.to_string()),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            };
            Some((Ok(event), (host, receiver, initial)))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
}
