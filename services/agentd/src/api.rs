//! HTTP API for the apps. Only `tailnet-proxy` can reach it (Unix socket), and
//! it sets `X-Tailscale-User` from the caller's Tailscale identity.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::agent::{Agent, StartError};
use crate::clock::UserClock;
use crate::llm::KNOWN_MODELS;

type App = State<Arc<Agent>>;

pub fn router(agent: Arc<Agent>) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/messages", get(messages).post(post_message))
        .route("/api/v1/events", get(events))
        .route("/api/v1/approvals/{id}", post(answer_approval))
        .route("/api/v1/actions/{id}", post(resolve_action))
        .route("/api/v1/stop", post(stop))
        .route("/api/v1/images/{id}", get(image))
        .route("/api/v1/screenshot", get(screenshot))
        .route("/api/v1/settings", get(settings).put(put_settings))
        .route("/api/v1/devices", post(register_device))
        .with_state(agent)
}

fn caller(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tailscale-user")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

async fn status(State(agent): App) -> Json<Value> {
    Json(json!({
        "desktop": agent.cfg.desktop_name,
        "busy": agent.is_busy(),
        "settings": agent.settings(),
        "last_seq": agent.store.last_seq().unwrap_or(0),
        "known_models": KNOWN_MODELS,
        "shell_tool": agent.cfg.shell_tool,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<i64>,
}

async fn messages(State(agent): App, Query(query): Query<Limit>) -> Response {
    match agent.store.history(query.limit.unwrap_or(50).clamp(1, 500)) {
        Ok(history) => Json(json!({"messages": history})).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct Device {
    /// The APNs device token, as hex.
    token: String,
    #[serde(default)]
    name: String,
}

/// The apps register for push notifications here.
async fn register_device(State(agent): App, headers: HeaderMap, Json(body): Json<Device>) -> Response {
    let token = body.token.trim().to_ascii_lowercase();
    if token.len() < 32 || token.len() > 200 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return error(StatusCode::BAD_REQUEST, "token must be the device token in hex");
    }
    let Some(relay) = agent.push.clone() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "push notifications aren't set up on this desktop",
        );
    };
    let name: String = body.name.chars().take(80).collect();
    match relay.register(&token, &name).await {
        Ok(()) => {
            tracing::info!(device = %name, by = ?caller(&headers), "device registered for push");
            Json(json!({"ok": true})).into_response()
        }
        Err(err) => {
            tracing::warn!(%err, "couldn't register a device with the push relay");
            error(StatusCode::BAD_GATEWAY, "couldn't reach the push relay")
        }
    }
}

#[derive(Deserialize)]
struct NewMessage {
    text: String,
    /// The app's time zone (IANA name) and current UTC offset in seconds, so
    /// the agent knows the user's date ("yesterday").
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    utc_offset: Option<i64>,
}

async fn post_message(State(agent): App, headers: HeaderMap, Json(body): Json<NewMessage>) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return error(StatusCode::BAD_REQUEST, "text is empty");
    }
    if let (Some(timezone), Some(offset)) = (&body.timezone, body.utc_offset)
        && let Some(clock) = UserClock::from_app(timezone, offset)
    {
        agent.set_user_clock(&clock);
    }
    match agent.start(text, caller(&headers)) {
        Ok(task) => (StatusCode::ACCEPTED, Json(json!({"task": task}))).into_response(),
        Err(_) => error(StatusCode::CONFLICT, "busy with another request; stop it first"),
    }
}

#[derive(Deserialize)]
struct After {
    after: Option<i64>,
}

/// Server-Sent Events: everything after `after` from the log, then live events.
async fn events(
    State(agent): App,
    Query(query): Query<After>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let live = BroadcastStream::new(agent.bus.subscribe()); // subscribe before reading the backlog
    let backlog = agent
        .store
        .events_after(query.after.unwrap_or(0), 5_000)
        .unwrap_or_default();
    let last = backlog.last().map_or(query.after.unwrap_or(0), |e| e.seq);
    let live = live.filter_map(move |event| event.ok().filter(|e| e.seq > last));
    let stream = tokio_stream::iter(backlog).chain(live).map(|event| {
        Ok(SseEvent::default()
            .id(event.seq.to_string())
            .event(event.kind.clone())
            .data(serde_json::to_string(&event).unwrap_or_default()))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct Approval {
    approved: bool,
}

async fn answer_approval(
    State(agent): App,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Approval>,
) -> Response {
    tracing::info!(id, approved = body.approved, by = ?caller(&headers), "approval answered");
    if agent.answer_approval(&id, body.approved, caller(&headers)) {
        Json(json!({"ok": true})).into_response()
    } else {
        error(
            StatusCode::NOT_FOUND,
            "no pending approval with that id (it may have expired)",
        )
    }
}

#[derive(Deserialize)]
struct ActionChoice {
    choice: String,
}

async fn resolve_action(
    State(agent): App,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ActionChoice>,
) -> Response {
    let done = match body.choice.as_str() {
        "done" => true,
        "skip" => false,
        _ => return error(StatusCode::BAD_REQUEST, "choice must be done or skip"),
    };
    match agent.resolve_action(&id, done, caller(&headers)) {
        Ok(task) => (StatusCode::ACCEPTED, Json(json!({"task": task}))).into_response(),
        Err(StartError::NotFound) => error(StatusCode::NOT_FOUND, "no pending action with that id"),
        Err(StartError::Busy) => error(StatusCode::CONFLICT, "busy with another request; stop it first"),
    }
}

async fn stop(State(agent): App) -> Json<Value> {
    Json(json!({"stopping": agent.stop()}))
}

async fn image(State(agent): App, Path(id): Path<String>) -> Response {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return error(StatusCode::BAD_REQUEST, "bad image id");
    }
    match tokio::fs::read(agent.cfg.state_dir.join("images").join(format!("{id}.jpg"))).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "no such image"),
    }
}

async fn screenshot(State(agent): App) -> Response {
    match agent.screenshot().await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        Err(err) => error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
    }
}

async fn settings(State(agent): App) -> Json<Value> {
    Json(json!(agent.settings()))
}

#[derive(Deserialize)]
struct SettingsUpdate {
    model: Option<String>,
    allow_actions: Option<bool>,
}

async fn put_settings(State(agent): App, Json(body): Json<SettingsUpdate>) -> Response {
    match agent.update_settings(body.model.as_deref(), body.allow_actions) {
        Ok(settings) => Json(json!(settings)).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}
