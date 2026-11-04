//! HTTP API for the apps. Only the desktop's way in can reach it (Unix socket):
//! `tailnet-proxy`, which sets `X-Tailscale-User` from the caller's Tailscale
//! identity, or in a customer deployment `front-door`, which sets
//! `X-Agent-User` from their Cloudflare Access sign-in. Each removes any copy
//! the caller sent.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::agent::{Agent, SecretError, StartError};
use crate::clock::UserClock;
use crate::llm::KNOWN_MODELS;
use crate::memory;
use crate::schedule;
use crate::store::Event;

type App = State<Arc<Agent>>;

pub fn router(agent: Arc<Agent>) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/messages", get(messages).post(post_message))
        .route("/api/v1/events", get(events))
        .route("/api/v1/events/page", get(events_page))
        .route("/api/v1/approvals/{id}", post(answer_approval))
        .route("/api/v1/actions/{id}", post(resolve_action))
        .route("/api/v1/stop", post(stop))
        .route("/api/v1/images/{id}", get(image))
        .route("/api/v1/screenshot", get(screenshot))
        .route("/api/v1/settings", get(settings).put(put_settings))
        .route("/api/v1/devices", post(register_device))
        .route("/api/v1/memory", get(memories))
        .route("/api/v1/memory/search", get(search_memory))
        .route("/api/v1/memory/consolidate", post(consolidate_now))
        .route("/api/v1/memory/consolidation", get(consolidation))
        .route("/api/v1/memory/export", get(export_memory))
        .route("/api/v1/memory/import", post(import_memory))
        .route("/api/v1/memory/{id}/restore", post(restore_memory))
        .route("/api/v1/memory/{id}", delete(forget_memory).patch(edit_memory))
        .route("/api/v1/memory/{id}/confirm", post(confirm_memory))
        .route("/api/v1/memory/{id}/approve", post(confirm_memory))
        .route("/api/v1/memory/{id}/history", get(memory_history))
        .route("/api/v1/schedules", get(schedules).post(add_schedule))
        .route("/api/v1/setup", get(get_setup).put(put_setup))
        .route("/api/v1/schedules/{id}", delete(delete_schedule))
        .route("/api/v1/schedules/{id}/pause", post(pause_schedule))
        .route("/api/v1/schedules/{id}/resume", post(resume_schedule))
        .route("/api/v1/secrets", get(list_secrets))
        .route("/api/v1/secrets/{name}", put(save_secret).delete(delete_secret))
        .route("/api/v1/destinations", get(list_destinations))
        .route(
            "/api/v1/destinations/{name}",
            put(save_destination).delete(delete_destination),
        )
        .route("/api/v1/destinations/{name}/test", post(test_destination))
        .with_state(agent)
}

/// Who made the request, for attribution (`by`), as the proxy in front set it.
fn caller(headers: &HeaderMap) -> Option<String> {
    ["x-tailscale-user", "x-agent-user"]
        .iter()
        .find_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()))
        .map(String::from)
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

/// What `stage` is worked out from.
#[derive(Default)]
pub struct StageFacts {
    /// Parked approvals: (id, card kind).
    pub cards: Vec<(String, String)>,
    /// The latest task: (id, status).
    pub latest: Option<(String, String)>,
    pub busy: bool,
    pub failing_effects: usize,
    /// The next schedule due within a day: (id, name, next_run).
    pub next_schedule: Option<(i64, String, i64)>,
}

/// One word for what the desktop needs, from the first matching row
/// (docs/memory-v2.md §12), and the ids behind it so the app can deep-link.
pub fn stage_of(facts: &StageFacts) -> (&'static str, Value) {
    let card = |kinds: &[&str]| {
        facts
            .cards
            .iter()
            .find(|(_, kind)| kinds.contains(&kind.as_str()))
    };
    let latest_is = |statuses: &[&str]| {
        facts
            .latest
            .as_ref()
            .filter(|(_, status)| statuses.contains(&status.as_str()))
    };
    if let Some((id, kind)) = card(&["shell", "browser", "schedule"]) {
        return ("needs_you/approval", json!({"card": id, "kind": kind}));
    }
    if let Some((id, _)) = card(&["secret"]) {
        return ("needs_you/secret", json!({"card": id}));
    }
    if let Some((run, _)) = latest_is(&["needs_user"]) {
        return ("needs_you/action", json!({"run": run}));
    }
    if facts.busy {
        return ("working", json!({"run": facts.latest.as_ref().map(|(id, _)| id)}));
    }
    if let Some((run, status)) = latest_is(&["error", "interrupted", "stuck", "max_steps"]) {
        return ("problem", json!({"run": run, "status": status}));
    }
    if facts.failing_effects > 0 {
        return ("problem", json!({"failing_effects": facts.failing_effects}));
    }
    if let Some((id, name, next_run)) = &facts.next_schedule {
        return (
            "scheduled",
            json!({"schedule": id, "name": name, "next_run": next_run}),
        );
    }
    ("idle", json!({}))
}

fn stage(agent: &Agent, failing_effects: usize) -> (&'static str, Value) {
    let now = crate::memory::unix_now();
    let next_schedule = agent
        .store
        .schedules()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.paused && s.next_run <= now + 86_400)
        .min_by_key(|s| s.next_run)
        .map(|s| (s.id, s.name, s.next_run));
    stage_of(&StageFacts {
        cards: agent.store.parked_cards().unwrap_or_default(),
        latest: agent.store.latest_task().ok().flatten(),
        busy: agent.is_busy(),
        failing_effects,
        next_schedule,
    })
}

async fn status(State(agent): App) -> Json<Value> {
    let (waiting, failing) = agent.store.effects_health().unwrap_or_default();
    let active_memories = agent.store.memory_counts().unwrap_or_default();
    let (stage, stage_detail) = stage(&agent, failing.len());
    let failing: Vec<Value> = failing
        .iter()
        .map(|e| json!({"kind": e.kind, "key": e.key, "attempts": e.attempts, "last_error": e.last_error}))
        .collect();
    Json(json!({
        "desktop": agent.cfg.desktop_name,
        "busy": agent.is_busy(),
        // The approval a parked task waits on.
        "waiting_on": agent.waiting_on(),
        "settings": agent.settings(),
        "last_seq": agent.store.last_seq().unwrap_or(0),
        "known_models": KNOWN_MODELS,
        "shell_tool": agent.cfg.shell_tool,
        "version": env!("CARGO_PKG_VERSION"),
        // The rules in force (policy.rs), and the side-effects queue's health.
        "policy_version": agent.policy_version(),
        "effects": {"pending": waiting, "failing": failing},
        "memory_v2": agent.cfg.memory_v2,
        "memory": {"active": active_memories},
        // One word for the app's headline, and what's behind it.
        "stage": stage,
        "stage_detail": stage_detail,
    }))
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<i64>,
}

async fn messages(State(agent): App, Query(query): Query<Limit>) -> Response {
    match agent.store.history(query.limit.unwrap_or(50).clamp(1, 500)) {
        Ok(mut history) => {
            for entry in &mut history {
                entry.text = agent.secrets.redact(&entry.text).into_owned();
            }
            Json(json!({"messages": history})).into_response()
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct Device {
    /// The APNs device token, as hex.
    token: String,
    #[serde(default)]
    name: String,
    /// The APNs servers the token belongs to: "sandbox" (Xcode installs) or
    /// "production" (TestFlight, the notarized Mac app). Older apps send none:
    /// the relay's default (sandbox).
    #[serde(default)]
    environment: Option<String>,
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
    let environment = match body.environment.as_deref() {
        None => None,
        Some(env @ ("sandbox" | "production")) => Some(env),
        Some(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "environment must be sandbox or production",
            );
        }
    };
    match relay.register(&token, &name, environment).await {
        Ok(()) => {
            tracing::info!(device = %name, environment = ?environment, by = ?caller(&headers), "device registered for push");
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
    /// Comma-separated event kinds; all when absent.
    kinds: Option<String>,
}

/// `a,b,c` → the kinds asked for (at most 64; none means all).
fn kinds(list: Option<&str>) -> Vec<String> {
    list.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .take(64)
        .map(String::from)
        .collect()
}

/// Server-Sent Events: everything after `after` from the log, then live events.
/// `kinds` keeps only those (the app's chat skips steps and command output).
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
    let kinds = kinds(query.kinds.as_deref());
    let wanted = move |event: &Event| kinds.is_empty() || kinds.contains(&event.kind);
    let backlog: Vec<Event> = backlog.into_iter().filter(&wanted).collect();
    let live = live.filter_map(move |event| event.ok().filter(|e| e.seq > last && wanted(e)));
    // Older events may hold a key pasted into the chat before it was saved.
    let stream = tokio_stream::iter(backlog).chain(live).map(move |mut event| {
        agent.secrets.redact_json(&mut event.data);
        Ok(SseEvent::default()
            .id(event.seq.to_string())
            .event(event.kind.clone())
            .data(serde_json::to_string(&event).unwrap_or_default()))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct PageQuery {
    before: Option<i64>,
    limit: Option<i64>,
    kinds: Option<String>,
}

/// The chat a page at a time: the apps load the newest page, then older ones
/// as the user scrolls up, and stream what comes next after `last_seq`.
async fn events_page(State(agent): App, Query(query): Query<PageQuery>) -> Response {
    let kinds = kinds(query.kinds.as_deref());
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    match agent.store.events_page(query.before, limit, &kinds) {
        Ok(mut page) => {
            // Older events may hold a key pasted into the chat before it was saved.
            for event in &mut page.events {
                agent.secrets.redact_json(&mut event.data);
            }
            Json(json!({"events": page.events, "more": page.more, "last_seq": page.last_seq})).into_response()
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
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
    match agent.answer_approval(&id, body.approved, caller(&headers)) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(StartError::NotFound) => error(
            StatusCode::NOT_FOUND,
            "no pending approval with that id (it may have expired)",
        ),
        Err(StartError::Busy) => error(StatusCode::CONFLICT, "busy with another request; stop it first"),
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

#[derive(Deserialize)]
struct MemoryQuery {
    /// Memory v2: comma-separated statuses (default `active,pending`).
    status: Option<String>,
    /// Memory v2: comma-separated kinds (default all).
    kind: Option<String>,
    /// Memory v2: full-text query.
    q: Option<String>,
    limit: Option<i64>,
}

fn comma_list(text: &Option<String>) -> Vec<&str> {
    text.as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// What the agent remembers, for the app's Memory screen. Memory v2 lists
/// active and pending items with all their fields (v1's fields included, so
/// older apps still read it).
async fn memories(State(agent): App, Query(query): Query<MemoryQuery>) -> Response {
    let limit = query.limit.unwrap_or(500);
    if agent.cfg.memory_v2 {
        let filter = memory::Filter {
            statuses: comma_list(&query.status),
            kinds: comma_list(&query.kind),
            query: query.q.as_deref(),
            limit,
        };
        return match memory::list_detailed(&agent.store, &filter) {
            Ok(memories) => Json(json!({"memories": memories})).into_response(),
            Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        };
    }
    match agent.store.memories(limit.clamp(1, 500)) {
        Ok(memories) => Json(json!({"memories": memories})).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    kinds: Option<String>,
    limit: Option<i64>,
}

/// Memory v2: active memories ranked as the agent's notes are (what its
/// `search_memory` tool sees), each with its score.
async fn search_memory(State(agent): App, Query(query): Query<SearchQuery>) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    let kinds = comma_list(&query.kinds);
    match memory::search(
        &agent.store,
        &query.q,
        &kinds,
        query.limit.unwrap_or(8).clamp(1, 50),
    ) {
        Ok(found) => {
            let results: Vec<Value> = found
                .into_iter()
                .map(|(item, score)| json!({"memory": item, "score": (score * 1000.0).round() / 1000.0}))
                .collect();
            Json(json!({"results": results})).into_response()
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Memory v2: consolidate now rather than 10 minutes after the last task
/// (it still waits for a running task to end).
async fn consolidate_now(State(agent): App, headers: HeaderMap) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    let waiting = crate::consolidate::batch(&agent.store).map_or(0, |(runs, _)| runs.len());
    let now = crate::memory::unix_now();
    match agent.enqueue("consolidate", "main", &json!({}), now) {
        Ok(()) => {
            tracing::info!(by = ?caller(&headers), waiting, "consolidation requested");
            Json(json!({"ok": true, "tasks": waiting, "mode": crate::consolidate::mode(&agent.store).name()}))
                .into_response()
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Memory v2: brings back an expired or replaced memory.
async fn restore_memory(State(agent): App, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    let by = caller(&headers);
    match memory::restore(&agent.store, memory::Actor::User { login: by.as_deref() }, id) {
        Ok(Some((item, replaced))) => {
            agent.emit(
                "memory.updated",
                json!({"id": id, "version": item.version, "by": by, "restored": true}),
            );
            if let Some(successor) = replaced {
                agent.emit(
                    "memory.superseded",
                    json!({"id": successor, "by_id": id, "by": by}),
                );
            }
            Json(json!({"memory": item})).into_response()
        }
        Ok(None) => error(
            StatusCode::NOT_FOUND,
            "no expired or replaced memory with that id",
        ),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

/// Memory v2: every memory as JSON (for an import), or the active ones as
/// Markdown (to read).
async fn export_memory(State(agent): App, Query(query): Query<ExportQuery>) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    match query.format.as_deref().unwrap_or("json") {
        "markdown" => match memory::export_markdown(&agent.store) {
            Ok(text) => ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], text).into_response(),
            Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        },
        "json" => match memory::export_json(&agent.store) {
            Ok(value) => Json(value).into_response(),
            Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        },
        _ => error(StatusCode::BAD_REQUEST, "format must be json or markdown"),
    }
}

/// Memory v2: imports a JSON export (its active memories) or a list of
/// `{text, kind?}`. Everything lands pending, for the user to approve.
async fn import_memory(State(agent): App, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    let by = caller(&headers);
    match memory::import(
        &agent.store,
        &agent.secrets,
        memory::Actor::User { login: by.as_deref() },
        &body,
    ) {
        Ok(done) => {
            tracing::info!(imported = done.imported, rejected = done.rejected.len(), by = ?by, "memories imported");
            Json(json!(done)).into_response()
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Memory v2: consolidation's settings, this month's cost and its last run.
async fn consolidation(State(agent): App) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    Json(crate::consolidate::stats(
        &agent.store,
        &agent.cfg.consolidation_model,
    ))
    .into_response()
}

/// Memory v2's endpoints answer this while the flag is off.
fn v2_off() -> Response {
    error(
        StatusCode::CONFLICT,
        "memory v2 is off on this desktop (MEMORY_V2)",
    )
}

async fn forget_memory(State(agent): App, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    let by = caller(&headers);
    let forgotten = if agent.cfg.memory_v2 {
        memory::forget(&agent.store, memory::Actor::User { login: by.as_deref() }, id).map(|m| m.is_some())
    } else {
        agent.store.forget(id)
    };
    match forgotten {
        Ok(true) => {
            agent.emit("memory.forgotten", json!({"id": id, "by": by}));
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => error(StatusCode::NOT_FOUND, "no such memory"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// The user vouches for an unverified memory (a hint): it becomes something
/// they said.
async fn confirm_memory(State(agent): App, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    let by = caller(&headers);
    if !agent.cfg.memory_v2 {
        return match agent.store.confirm_memory(id) {
            Ok(true) => {
                agent.emit("memory.confirmed", json!({"id": id, "by": by}));
                Json(json!({"ok": true})).into_response()
            }
            Ok(false) => error(StatusCode::NOT_FOUND, "no such memory"),
            Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        };
    }
    match memory::approve(&agent.store, memory::Actor::User { login: by.as_deref() }, id) {
        Ok(Some(item)) => {
            agent.emit("memory.confirmed", json!({"id": id, "by": by}));
            Json(json!({"ok": true, "memory": item})).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "no unverified memory with that id"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct MemoryEdit {
    text: Option<String>,
    kind: Option<String>,
    /// `YYYY-MM-DD` (end of that day, the user's time), or null to clear.
    #[serde(default, deserialize_with = "double_option")]
    valid_until: Option<Option<String>>,
    importance: Option<f64>,
    expected_version: i64,
}

/// Tells a missing field (None) from an explicit null (Some(None)).
fn double_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// A user's edit: 409 with the current memory when it changed meanwhile.
async fn edit_memory(
    State(agent): App,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(body): Json<MemoryEdit>,
) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    let valid_until = match body.valid_until {
        None => None,
        Some(None) => Some(None),
        Some(Some(date)) => match agent.end_of_day(&date) {
            Some(until) => Some(Some(until)),
            None => return error(StatusCode::BAD_REQUEST, "valid_until must be a date, YYYY-MM-DD"),
        },
    };
    let by = caller(&headers);
    let change = memory::Edit {
        text: body.text,
        kind: body.kind,
        valid_until,
        importance: body.importance,
    };
    let actor = memory::Actor::User { login: by.as_deref() };
    match memory::edit(
        &agent.store,
        &agent.secrets,
        actor,
        id,
        change,
        body.expected_version,
    ) {
        Ok(memory::Edited::Done(item)) => {
            agent.emit(
                "memory.updated",
                json!({"id": id, "version": item.version, "by": by}),
            );
            Json(json!({"memory": item})).into_response()
        }
        Ok(memory::Edited::Changed(item)) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "it changed meanwhile", "memory": item})),
        )
            .into_response(),
        Ok(memory::Edited::NotFound) => error(StatusCode::NOT_FOUND, "no such memory"),
        Ok(memory::Edited::Refused(why)) => error(StatusCode::BAD_REQUEST, &why),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn memory_history(State(agent): App, Path(id): Path<i64>) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    match memory::history(&agent.store, id) {
        Ok(changes) => Json(json!({"history": changes})).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn get_setup(State(agent): App) -> Json<Value> {
    Json(json!({"steps": crate::agent::SETUP_STEPS, "skipped": agent.setup_skipped()}))
}

#[derive(Deserialize)]
struct SetupUpdate {
    skipped: Vec<String>,
}

/// The Setup steps skipped, all of them (a step left out isn't skipped).
async fn put_setup(State(agent): App, headers: HeaderMap, Json(body): Json<SetupUpdate>) -> Response {
    match agent.set_setup_skipped(&body.skipped, caller(&headers)) {
        Ok(skipped) => Json(json!({"steps": crate::agent::SETUP_STEPS, "skipped": skipped})).into_response(),
        Err(problem) => error(StatusCode::BAD_REQUEST, &problem),
    }
}

#[derive(Deserialize)]
struct NewSchedule {
    name: String,
    prompt: String,
    cron: String,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    utc_offset: Option<i64>,
}

/// A repeating schedule set up in the web app (Setup's examples).
async fn add_schedule(State(agent): App, headers: HeaderMap, Json(body): Json<NewSchedule>) -> Response {
    let clock = match (body.timezone.as_deref(), body.utc_offset) {
        (Some(timezone), Some(offset)) => crate::clock::UserClock::from_app(timezone, offset),
        _ => None,
    };
    match agent.add_user_schedule(
        &body.name,
        &body.prompt,
        &body.cron,
        clock.as_ref(),
        caller(&headers),
    ) {
        Ok(schedule) => Json(json!({"schedule": schedule})).into_response(),
        Err(problem) => error(StatusCode::BAD_REQUEST, &problem),
    }
}

async fn schedules(State(agent): App) -> Response {
    match agent.store.schedules() {
        Ok(schedules) => {
            let schedules: Vec<Value> = schedules.iter().map(schedule::json).collect();
            Json(json!({"schedules": schedules})).into_response()
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn delete_schedule(State(agent): App, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    match agent.store.delete_schedule(id) {
        Ok(true) => {
            agent.emit("schedule.deleted", json!({"id": id, "by": caller(&headers)}));
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => error(StatusCode::NOT_FOUND, "no such schedule"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn pause_schedule(State(agent): App, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    set_paused(&agent, id, true, &headers)
}

/// A resumed schedule picks up from now: runs it missed while paused are skipped.
async fn resume_schedule(State(agent): App, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    set_paused(&agent, id, false, &headers)
}

fn set_paused(agent: &Agent, id: i64, paused: bool, headers: &HeaderMap) -> Response {
    let schedule = match agent.store.schedule(id) {
        Ok(Some(schedule)) => schedule,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such schedule"),
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let now = jiff::Timestamp::now();
    let next_run = match (&schedule.cron, paused) {
        (Some(cron), false) => schedule::next_run(cron, &schedule.timezone, now),
        _ => None,
    };
    if !paused && schedule.cron.is_none() && schedule.next_run < now.as_second() {
        return error(StatusCode::CONFLICT, "its time has passed");
    }
    match agent.store.set_schedule_paused(id, paused, next_run) {
        Ok(true) => {
            agent.schedules_changed.notify_one();
            let updated = agent.store.schedule(id).ok().flatten();
            let mut event = updated.as_ref().map_or_else(|| json!({"id": id}), schedule::json);
            event["by"] = json!(caller(headers));
            agent.emit("schedule.updated", event.clone());
            Json(event).into_response()
        }
        Ok(false) => error(StatusCode::NOT_FOUND, "no such schedule"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// The saved keys, by name and label. Values never leave the desktop.
async fn list_secrets(State(agent): App) -> Json<Value> {
    Json(json!({"secrets": agent.secret_infos()}))
}

#[derive(Deserialize)]
struct NewSecret {
    value: String,
    #[serde(default)]
    label: Option<String>,
    /// The request card it answers: that task resumes.
    #[serde(default)]
    approval: Option<String>,
}

/// Saves a key from the app (typed or pasted into a secure field). The body
/// is never logged.
async fn save_secret(
    State(agent): App,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<NewSecret>,
) -> Response {
    let by = caller(&headers);
    let info = match agent.save_secret(&name, &body.value, body.label.as_deref(), by.clone()) {
        Ok(info) => info,
        Err(SecretError::Invalid(problem)) => return error(StatusCode::BAD_REQUEST, &problem),
        Err(SecretError::Failed(err)) => {
            tracing::error!(%err, name, "couldn't save a key");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "couldn't save the key");
        }
    };
    // Resumes the task that asked for it; the key stays saved either way.
    let (resumed, busy) = match body.approval {
        Some(id) => match agent.answer_secret_request(&id, &name, by) {
            Ok(()) => (true, false),
            Err(StartError::Busy) => (false, true),
            Err(StartError::NotFound) => (false, false),
        },
        None => (false, false),
    };
    Json(json!({"secret": info, "resumed": resumed, "busy": busy})).into_response()
}

async fn delete_secret(State(agent): App, Path(name): Path<String>, headers: HeaderMap) -> Response {
    match agent.delete_secret(&name, caller(&headers)) {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "no such key"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn list_destinations(State(agent): App) -> Response {
    match agent.store.destinations() {
        Ok(destinations) => Json(json!({"destinations": destinations})).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct NewDestination {
    kind: String,
    #[serde(default)]
    channel: String,
    /// The Slack bot token or the Teams webhook URL; absent keeps the saved one.
    #[serde(default)]
    credential: Option<String>,
    /// The web app's own address, for the link back in each post.
    #[serde(default)]
    link: String,
}

/// From the web app. The body (it holds a token or a webhook URL) is never logged.
async fn save_destination(
    State(agent): App,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<NewDestination>,
) -> Response {
    match agent.save_destination(
        &name,
        &body.kind,
        &body.channel,
        body.credential.as_deref(),
        &body.link,
        caller(&headers),
    ) {
        Ok(destination) => Json(json!({"destination": destination})).into_response(),
        Err(problem) => error(StatusCode::BAD_REQUEST, &problem),
    }
}

async fn delete_destination(State(agent): App, Path(name): Path<String>, headers: HeaderMap) -> Response {
    match agent.delete_destination(&name, caller(&headers)) {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "no such destination"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Posts a fixed test message: 502 with the reason, in plain words, if it fails.
async fn test_destination(State(agent): App, Path(name): Path<String>) -> Response {
    match agent.test_destination(&name).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(problem) => error(StatusCode::BAD_GATEWAY, &problem),
    }
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
    /// "root" or "risky" (agent::APPROVALS).
    approvals: Option<String>,
    memory_consolidation: Option<String>,
    memory_consolidation_model: Option<String>,
    memory_consolidation_daily_cap_usd: Option<f64>,
}

async fn put_settings(State(agent): App, Json(body): Json<SettingsUpdate>) -> Response {
    if let Some(approvals) = body.approvals.as_deref()
        && !crate::agent::APPROVALS.contains(&approvals)
    {
        return error(StatusCode::BAD_REQUEST, "approvals must be root or risky");
    }
    let memory = agent.update_memory_settings(
        body.memory_consolidation.as_deref(),
        body.memory_consolidation_model.as_deref(),
        body.memory_consolidation_daily_cap_usd,
    );
    match memory {
        Ok(Err(problem)) => return error(StatusCode::BAD_REQUEST, &problem),
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        Ok(Ok(_)) => {}
    }
    match agent.update_settings(
        body.model.as_deref(),
        body.allow_actions,
        body.approvals.as_deref(),
    ) {
        Ok(settings) => Json(json!(settings)).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_caller_comes_from_either_proxy() {
        let mut headers = HeaderMap::new();
        assert_eq!(caller(&headers), None);
        headers.insert("x-agent-user", "me@acme.com".parse().unwrap());
        assert_eq!(caller(&headers).as_deref(), Some("me@acme.com"));
        // tailnet-proxy only strips its own headers, so its identity wins.
        headers.insert("x-tailscale-user", "me@example.com".parse().unwrap());
        assert_eq!(caller(&headers).as_deref(), Some("me@example.com"));
    }

    fn with(change: impl FnOnce(&mut StageFacts)) -> &'static str {
        let mut facts = StageFacts::default();
        change(&mut facts);
        stage_of(&facts).0
    }

    #[test]
    fn the_stage_is_the_first_row_that_matches() {
        assert_eq!(with(|_| {}), "idle");
        assert_eq!(
            with(|f| f.next_schedule = Some((4, "Morning inbox".into(), 100))),
            "scheduled"
        );
        assert_eq!(with(|f| f.failing_effects = 1), "problem");
        for status in ["error", "interrupted", "stuck", "max_steps"] {
            assert_eq!(
                with(|f| f.latest = Some(("t1".into(), status.into()))),
                "problem",
                "{status}"
            );
        }
        assert_eq!(with(|f| f.latest = Some(("t1".into(), "done".into()))), "idle");
        assert_eq!(with(|f| f.busy = true), "working");
        assert_eq!(
            with(|f| f.latest = Some(("t1".into(), "needs_user".into()))),
            "needs_you/action"
        );
        assert_eq!(
            with(|f| f.cards = vec![("s1".into(), "secret".into())]),
            "needs_you/secret"
        );
        for kind in ["shell", "browser", "schedule"] {
            assert_eq!(
                with(|f| f.cards = vec![("a1".into(), kind.into())]),
                "needs_you/approval"
            );
        }
    }

    #[test]
    fn kinds_are_a_comma_list() {
        assert!(kinds(None).is_empty());
        assert!(kinds(Some(" , ")).is_empty());
        assert_eq!(kinds(Some("message, task.started,")), ["message", "task.started"]);
        assert_eq!(kinds(Some(&"k,".repeat(100))).len(), 64);
    }

    #[test]
    fn what_needs_the_user_comes_first() {
        let everything = StageFacts {
            cards: vec![("s1".into(), "secret".into()), ("a1".into(), "browser".into())],
            latest: Some(("t9".into(), "needs_user".into())),
            busy: true,
            failing_effects: 3,
            next_schedule: Some((4, "Morning inbox".into(), 100)),
        };
        assert_eq!(
            stage_of(&everything),
            ("needs_you/approval", json!({"card": "a1", "kind": "browser"}))
        );
        let working = StageFacts {
            cards: vec![],
            latest: Some(("t9".into(), "error".into())),
            ..everything
        };
        assert_eq!(stage_of(&working).0, "working");
        let problem = StageFacts {
            busy: false,
            ..working
        };
        assert_eq!(
            stage_of(&problem),
            ("problem", json!({"run": "t9", "status": "error"}))
        );
    }
}
