//! HTTP API for the web app. Only the desktop's way in can reach it (Unix
//! socket): `tailnet-proxy`, which sets `X-Tailscale-User` from the caller's
//! Tailscale identity and removes any copy the caller sent.

use std::collections::VecDeque;
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

use crate::agent::{Agent, Edit, SecretError, StartError};
use crate::clock::UserClock;
use crate::llm;
use crate::memory;
use crate::schedule;
use crate::sessions::{PostError, SessionError};
use crate::store::{Event, MAIN};

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
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/v1/sessions/{id}",
            get(get_session).patch(update_session).delete(delete_session),
        )
        .route(
            "/api/v1/sessions/{id}/messages",
            get(session_messages).post(post_session_message),
        )
        .route("/api/v1/sessions/{id}/queue/{qid}", delete(unqueue))
        .route("/api/v1/sessions/{id}/stop", post(stop_session))
        .route("/api/v1/usage", get(usage))
        .route("/api/v1/images/{id}", get(image))
        .route("/api/v1/settings", get(settings).put(put_settings))
        .route("/api/v1/settings/auto-approve", put(set_auto_approve))
        .route("/api/v1/memory", get(memories))
        .route("/api/v1/memory/consolidate", post(consolidate_now))
        .route("/api/v1/memory/consolidation", get(consolidation))
        .route("/api/v1/memory/export", get(export_memory))
        .route("/api/v1/memory/import", post(import_memory))
        .route("/api/v1/memory/{id}/restore", post(restore_memory))
        .route("/api/v1/memory/{id}", delete(forget_memory).patch(edit_memory))
        .route("/api/v1/memory/{id}/confirm", post(confirm_memory))
        .route("/api/v1/memory/{id}/history", get(memory_history))
        .route("/api/v1/schedules", get(schedules).post(add_schedule))
        .route(
            "/api/v1/schedules/{id}",
            delete(delete_schedule).put(update_schedule),
        )
        .route("/api/v1/schedules/{id}/pause", post(pause_schedule))
        .route("/api/v1/schedules/{id}/resume", post(resume_schedule))
        .route("/api/v1/push", get(push_info))
        .route("/api/v1/push/subscriptions", post(subscribe_push))
        .route("/api/v1/push/subscriptions/{id}", delete(unsubscribe_push))
        .route("/api/v1/push/test", post(test_push))
        .route("/api/v1/devices", post(register_device))
        .route("/api/v1/devices/{id}", delete(unregister_device))
        .route("/api/v1/model-keys", get(model_keys))
        .route(
            "/api/v1/model-keys/{provider}",
            put(save_model_key).delete(delete_model_key),
        )
        .route("/api/v1/secrets", get(list_secrets))
        .route("/api/v1/secrets/{name}", put(save_secret).delete(delete_secret))
        .route("/api/v1/access", get(access_list).post(allow_host))
        .route("/api/v1/access/{host}", delete(remove_host))
        .with_state(agent)
}

/// Who made the request, for attribution (`by`), as the proxy in front set it.
fn caller(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tailscale-user")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

/// What `stage` is worked out from, across every conversation.
#[derive(Default)]
pub struct StageFacts {
    /// Parked approvals: (id, card kind, conversation).
    pub cards: Vec<(String, String, String)>,
    /// Open Action needed cards, each its conversation's latest task's:
    /// (run, conversation).
    pub actions: Vec<(String, String)>,
    /// The latest task: (id, status, conversation).
    pub latest: Option<(String, String, String)>,
    /// The conversations working: (conversation, task).
    pub working: Vec<(String, String)>,
    pub failing_effects: usize,
    /// The next schedule due within a day: (id, name, next_run).
    pub next_schedule: Option<(i64, String, i64)>,
}

/// One word for what the desktop needs, from the first matching row
/// (docs/memory-v2.md §12), and the ids behind it (with the conversation)
/// so the app can deep-link.
pub fn stage_of(facts: &StageFacts) -> (&'static str, Value) {
    let card = |kinds: &[&str]| {
        facts
            .cards
            .iter()
            .find(|(_, kind, _)| kinds.contains(&kind.as_str()))
    };
    let latest_is = |statuses: &[&str]| {
        facts
            .latest
            .as_ref()
            .filter(|(_, status, _)| statuses.contains(&status.as_str()))
    };
    // Every card that waits for an OK: a command, the browser, a native app,
    // a schedule, a site to allow, or a task that looks stuck.
    if let Some((id, kind, session)) = card(&[
        "shell", "browser", "app", "install", "schedule", "access", "stuck",
    ]) {
        return (
            "needs_you/approval",
            json!({"card": id, "kind": kind, "session": session}),
        );
    }
    if let Some((id, _, session)) = card(&["secret"]) {
        return ("needs_you/secret", json!({"card": id, "session": session}));
    }
    if let Some((run, session)) = facts.actions.first() {
        return ("needs_you/action", json!({"run": run, "session": session}));
    }
    if let Some((run, _, session)) = latest_is(&["needs_user"]) {
        return ("needs_you/action", json!({"run": run, "session": session}));
    }
    if let Some((session, run)) = facts.working.first() {
        return ("working", json!({"run": run, "session": session}));
    }
    if let Some((run, status, session)) = latest_is(&["error", "interrupted", "stuck", "max_steps"]) {
        return (
            "problem",
            json!({"run": run, "status": status, "session": session}),
        );
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
        actions: agent.store.open_actions().unwrap_or_default(),
        latest: agent.store.latest_task().ok().flatten(),
        working: agent.working(),
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
        // `main` is working, for older apps (scheduled runs go beside the
        // conversations: background).
        "busy": agent.is_busy(),
        // How many conversations work and wait, and how many messages wait.
        "sessions": agent.sessions_status(),
        "budget": agent.budget().json(),
        "background": {
            "running": agent.background_run().map(|(task, schedule)| json!({"task": task, "schedule": schedule})),
            "queued": agent.store.queued_count().unwrap_or(0),
        },
        // The approval a parked task waits on.
        "waiting_on": agent.waiting_on(),
        "settings": agent.settings(),
        "last_seq": agent.store.last_seq().unwrap_or(0),
        // The models tasks can use now (older apps list these).
        "known_models": agent.usable_models(),
        // The chat model's provider.
        "provider": llm::Provider::of(&agent.settings().model).unwrap_or(agent.cfg.provider).name(),
        // Every provider, its models and where its key comes from (agentd
        // 0.20.0); and the chat model's provider when it has no key yet, so
        // the apps ask for one.
        "model_providers": agent.model_providers(),
        "needs_model_key": agent.needs_model_key().map(|p| json!({"id": p.id(), "name": p.name()})),
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

/// `main`'s history (older apps).
async fn messages(State(agent): App, Query(query): Query<Limit>) -> Response {
    history(&agent, MAIN, query.limit)
}

/// A conversation's history, with saved keys redacted.
fn history(agent: &Agent, session: &str, limit: Option<i64>) -> Response {
    match agent.store.history_in(session, limit.unwrap_or(50).clamp(1, 500)) {
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
struct NewMessage {
    text: String,
    /// The app's time zone (IANA name) and current UTC offset in seconds, so
    /// the agent knows the user's date ("yesterday").
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    utc_offset: Option<i64>,
}

/// A message for `main` (older apps): as `POST /sessions/main/messages`.
async fn post_message(State(agent): App, headers: HeaderMap, Json(body): Json<NewMessage>) -> Response {
    post_to(&agent, MAIN, &headers, body)
}

/// A message for a conversation: 202 `{task}` when it starts, or 202
/// `{queued: {id, position}}` when it waits its turn.
fn post_to(agent: &Arc<Agent>, session: &str, headers: &HeaderMap, body: NewMessage) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return error(StatusCode::BAD_REQUEST, "text is empty");
    }
    if let (Some(timezone), Some(offset)) = (&body.timezone, body.utc_offset)
        && let Some(clock) = UserClock::from_app(timezone, offset)
    {
        agent.set_user_clock(&clock);
    }
    match agent.post(session, text, caller(headers)) {
        Ok(posted) => (StatusCode::ACCEPTED, Json(posted.json())).into_response(),
        Err(PostError::NotFound) => error(
            StatusCode::NOT_FOUND,
            "no such conversation (it may be archived or deleted)",
        ),
        Err(PostError::NeedsKey(provider)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "Your agent needs your {} API key before it can work: add it in Settings, under Model",
                    provider.name()
                ),
                "needs_model_key": {"id": provider.id(), "name": provider.name()},
            })),
        )
            .into_response(),
        Err(PostError::Failed(err)) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

#[derive(Deserialize)]
struct After {
    after: Option<i64>,
    /// Comma-separated event kinds; all when absent.
    kinds: Option<String>,
    /// Only this conversation's events, and the global ones.
    session: Option<String>,
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
    let mut cursor = EventCursor::new(
        agent.clone(),
        query.after.unwrap_or(0),
        kinds(query.kinds.as_deref()),
    );
    cursor.session = query.session.filter(|s| !s.is_empty());
    // Older events may hold a key pasted into the chat before it was saved.
    let stream = futures_util::stream::unfold(cursor, |mut cursor| async {
        let event = cursor.next().await?;
        Some((event, cursor))
    })
    .map(move |mut event| {
        agent.secrets.redact_json(&mut event.data);
        Ok(SseEvent::default()
            .id(event.seq.to_string())
            .event(event.kind.clone())
            .data(serde_json::to_string(&event).unwrap_or_default()))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// SQLite owns the stream order. The broadcast only wakes a cursor that's caught up.
struct EventCursor {
    agent: Arc<Agent>,
    live: tokio::sync::broadcast::Receiver<Event>,
    after: i64,
    kinds: Vec<String>,
    /// Only this conversation's events and the global ones; all when None.
    session: Option<String>,
    batch: VecDeque<Event>,
}

impl EventCursor {
    fn new(agent: Arc<Agent>, after: i64, kinds: Vec<String>) -> Self {
        let live = agent.bus.subscribe();
        Self {
            agent,
            live,
            after,
            kinds,
            session: None,
            batch: VecDeque::new(),
        }
    }

    fn wanted(&self, event: &Event) -> bool {
        (self.kinds.is_empty() || self.kinds.contains(&event.kind))
            && (self.session.is_none() || event.session.is_none() || event.session == self.session)
    }

    async fn next(&mut self) -> Option<Event> {
        loop {
            while let Some(event) = self.batch.pop_front() {
                self.after = event.seq;
                if self.wanted(&event) {
                    return Some(event);
                }
            }
            match self.agent.store.events_after(self.after, 256) {
                Ok(events) if !events.is_empty() => {
                    self.batch = events.into();
                    tokio::task::yield_now().await;
                }
                Ok(_) => loop {
                    use tokio::sync::broadcast::error::RecvError;
                    match self.live.recv().await {
                        Ok(event) if event.seq <= self.after => continue,
                        Ok(_) | Err(RecvError::Lagged(_)) => break,
                        Err(RecvError::Closed) => return None,
                    }
                },
                Err(err) => {
                    tracing::warn!(%err, after = self.after, "couldn't read event stream");
                    return None;
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct PageQuery {
    before: Option<i64>,
    limit: Option<i64>,
    kinds: Option<String>,
    /// One conversation's events and the global ones; everything without.
    session: Option<String>,
}

/// The chat a page at a time: the apps load the newest page, then older ones
/// as the user scrolls up, and stream what comes next after `last_seq`.
async fn events_page(State(agent): App, Query(query): Query<PageQuery>) -> Response {
    let kinds = kinds(query.kinds.as_deref());
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let session = query.session.as_deref().filter(|s| !s.is_empty());
    match agent.store.events_page_in(query.before, limit, &kinds, session) {
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
    /// What the user changed the card's text to (a command, a message, a
    /// schedule's instructions): it replaces the agent's.
    text: Option<String>,
    /// A plan card's steps as the user edited them: they replace the agent's.
    plan: Option<Value>,
    /// "Allow for this task": the card's kind of action goes ahead without
    /// asking for the rest of the task.
    #[serde(default)]
    for_task: bool,
}

async fn answer_approval(
    State(agent): App,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Approval>,
) -> Response {
    let edited = body.text.as_deref().filter(|_| body.approved);
    let plan = body.plan.as_ref().filter(|_| body.approved);
    tracing::info!(id, approved = body.approved, edited = edited.is_some() || plan.is_some(), by = ?caller(&headers), "approval answered");
    if edited.is_some() && plan.is_some() {
        return error(StatusCode::BAD_REQUEST, "send text or plan, not both");
    }
    if let Some(problem) = edited.and_then(|text| agent.edit_problem(&id, text)) {
        return error(StatusCode::BAD_REQUEST, problem);
    }
    if let Some(problem) = plan.and_then(|steps| agent.plan_problem(&id, steps)) {
        return error(StatusCode::BAD_REQUEST, &problem);
    }
    let edit = edited.map(Edit::Text).or(plan.map(Edit::Plan));
    match agent.answer_approval_for(&id, body.approved, edit, body.for_task, caller(&headers)) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(StartError::NotFound) => error(
            StatusCode::NOT_FOUND,
            "no pending approval with that id (it may have expired)",
        ),
        // Only when another card of the same conversation was just answered.
        Err(StartError::Busy) => error(
            StatusCode::CONFLICT,
            "another card of this conversation was just answered; try again when it's done",
        ),
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
    // Starts the resume in the card's conversation, or queues it.
    match agent.resolve_action(&id, done, caller(&headers)) {
        Ok(posted) => (StatusCode::ACCEPTED, Json(posted.json())).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "no pending action with that id"),
    }
}

/// `main`'s Stop (older apps): `stopping` as before, with `stopped` and
/// `dropped` (queued messages) beside it.
async fn stop(State(agent): App) -> Json<Value> {
    Json(agent.stop(MAIN).json())
}

#[derive(Deserialize)]
struct ArchivedQuery {
    /// `1`: the archived conversations instead.
    archived: Option<String>,
}

/// The conversations, most recently updated first, and how many may work
/// at once.
async fn list_sessions(State(agent): App, Query(query): Query<ArchivedQuery>) -> Response {
    let archived = matches!(query.archived.as_deref(), Some("1" | "true"));
    match agent.sessions_view(archived) {
        Ok(sessions) => {
            Json(json!({"sessions": sessions, "running_limit": agent.cfg.max_parallel_tasks})).into_response()
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

#[derive(Deserialize)]
struct NewSession {
    #[serde(default)]
    title: Option<String>,
}

/// "New conversation": an empty one.
async fn create_session(State(agent): App, body: Option<Json<NewSession>>) -> Response {
    let title = body.and_then(|Json(body)| body.title);
    match agent.create_session(title.as_deref()) {
        Ok(session) => (StatusCode::CREATED, Json(session)).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

async fn get_session(State(agent): App, Path(id): Path<String>) -> Response {
    match agent.session_view(&id) {
        Some(session) => Json(session).into_response(),
        None => error(StatusCode::NOT_FOUND, "no such conversation"),
    }
}

#[derive(Deserialize)]
struct SessionChange {
    title: Option<String>,
    archived: Option<bool>,
}

fn session_error(err: SessionError) -> Response {
    match err {
        SessionError::NotFound => error(StatusCode::NOT_FOUND, "no such conversation"),
        SessionError::Conflict(why) => error(StatusCode::CONFLICT, why),
        SessionError::Failed(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// Renames, archives or brings back a conversation.
async fn update_session(
    State(agent): App,
    Path(id): Path<String>,
    Json(body): Json<SessionChange>,
) -> Response {
    match agent.update_session(&id, body.title.as_deref(), body.archived) {
        Ok(session) => Json(session).into_response(),
        Err(err) => session_error(err),
    }
}

async fn delete_session(State(agent): App, Path(id): Path<String>) -> Response {
    match agent.delete_session(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => session_error(err),
    }
}

async fn session_messages(State(agent): App, Path(id): Path<String>, Query(query): Query<Limit>) -> Response {
    if agent.session_view(&id).is_none() {
        return error(StatusCode::NOT_FOUND, "no such conversation");
    }
    history(&agent, &id, query.limit)
}

async fn post_session_message(
    State(agent): App,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<NewMessage>,
) -> Response {
    post_to(&agent, &id, &headers, body)
}

/// The ✕ on a queued message.
async fn unqueue(State(agent): App, Path((id, qid)): Path<(String, String)>) -> Response {
    match agent.unqueue(&id, &qid) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(
            StatusCode::NOT_FOUND,
            "no such queued message (it may have started)",
        ),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// A conversation's Stop: its task, or the card it waits on, and its queue.
async fn stop_session(State(agent): App, Path(id): Path<String>) -> Response {
    if agent.session_view(&id).is_none() {
        return error(StatusCode::NOT_FOUND, "no such conversation");
    }
    Json(agent.stop(&id).json()).into_response()
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<i64>,
}

/// What the model has cost (estimates from token counts), and the budget.
async fn usage(State(agent): App, Query(query): Query<UsageQuery>) -> Response {
    match agent.usage(query.days.unwrap_or(30)) {
        Ok(usage) => Json(usage).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
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

/// What the agent remembers: the apps' Memory screen (and the evals). Memory
/// v2 lists active items by default, with all their fields (v1's included).
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

/// Memory v2: consolidate now rather than 10 minutes after the last task
/// (it still waits for a running task to end).
async fn consolidate_now(State(agent): App, headers: HeaderMap) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    let waiting = crate::consolidate::batch(&agent.store).map_or(0, |runs| runs.len());
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

/// Memory v2: consolidation's settings, this month's cost and its last run.
async fn consolidation(State(agent): App) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    Json(agent.consolidation_stats()).into_response()
}

/// Memory v2's endpoints answer this while the flag is off.
fn v2_off() -> Response {
    error(
        StatusCode::CONFLICT,
        "memory v2 is off on this desktop (MEMORY_V2)",
    )
}

// The apps' Memory screen (apps/apple): forget, confirm, edit and restore a
// memory, export and import. The web app has none of it; there, you ask the
// agent.

/// Memory v2: brings back an expired or replaced memory.
async fn restore_memory(State(agent): App, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    let by = caller(&headers);
    match memory::restore(&agent.store, memory::Actor::User { login: by.as_deref() }, id) {
        Ok(memory::Restored::Done { item, replaced }) => {
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
        Ok(memory::Restored::NotFound) => error(
            StatusCode::NOT_FOUND,
            "no expired or replaced memory with that id",
        ),
        // Like the other limits here (sites): the request is fine, the state isn't.
        Ok(memory::Restored::Full) => error(
            StatusCode::CONFLICT,
            &format!(
                "too many active memories ({}): forget some before restoring this one",
                memory::MAX_ACTIVE
            ),
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
/// `{text, kind?}`.
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

async fn forget_memory(State(agent): App, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    let by = caller(&headers);
    match memory::forget(&agent.store, memory::Actor::User { login: by.as_deref() }, id) {
        Ok(memory::Forgot::Forgotten(_)) => {
            agent.emit("memory.forgotten", json!({"id": id, "by": by}));
            Json(json!({"ok": true})).into_response()
        }
        Ok(_) => error(StatusCode::NOT_FOUND, "no such memory"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// The user vouches for an unverified memory (a hint): it becomes something
/// they said.
async fn confirm_memory(State(agent): App, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    let by = caller(&headers);
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
fn double_option<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
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

/// Memory v2: what happened to a memory, newest first.
async fn memory_history(State(agent): App, Path(id): Path<i64>) -> Response {
    if !agent.cfg.memory_v2 {
        return v2_off();
    }
    match memory::history(&agent.store, id) {
        Ok(changes) => Json(json!({"history": changes})).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
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
    /// "Run without asking": on unless the request turns it off. A person
    /// set it up, so its runs don't wait for them (§6.14).
    #[serde(default = "run_without_asking")]
    auto_approve: bool,
}

fn run_without_asking() -> bool {
    true
}

#[derive(Deserialize)]
struct ScheduleChange {
    auto_approve: Option<bool>,
}

/// A person changes a schedule in the web app: "Run without asking".
async fn update_schedule(
    State(agent): App,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(body): Json<ScheduleChange>,
) -> Response {
    let Some(on) = body.auto_approve else {
        return error(StatusCode::BAD_REQUEST, "nothing to change (auto_approve)");
    };
    match agent.set_schedule_auto_approve(id, on, caller(&headers)) {
        Ok(Some(schedule)) => Json(schedule).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such schedule"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// A repeating schedule set up in the web app.
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
        body.auto_approve,
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
            agent.stop_schedule_runs(id);
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
            if paused {
                agent.stop_schedule_runs(id);
            }
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

/// The sites the shell can reach: the deployment's and the user's (access.rs).
async fn access_list(State(agent): App) -> Json<Value> {
    Json(agent.access_list())
}

#[derive(Deserialize)]
struct AllowHost {
    host: String,
}

/// A person adds a site in Settings.
async fn allow_host(State(agent): App, headers: HeaderMap, Json(body): Json<AllowHost>) -> Response {
    if !agent.access_list()["enabled"].as_bool().unwrap_or(false) {
        return error(
            StatusCode::CONFLICT,
            "this desktop's sites are set by its deployment",
        );
    }
    let host = match crate::access::check_host(&body.host) {
        Ok(host) => host,
        Err(problem) => return error(StatusCode::BAD_REQUEST, &problem),
    };
    let list = agent.access_list();
    let open: Vec<String> = list["base"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .chain(
            list["allowed"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|h| h["host"].as_str().map(String::from)),
        )
        .collect();
    if crate::access::covered(&host, &open) {
        return error(StatusCode::CONFLICT, &format!("{host} is already allowed"));
    }
    if list["allowed"].as_array().map_or(0, Vec::len) >= crate::access::MAX_HOSTS {
        return error(StatusCode::CONFLICT, "too many sites: remove some first");
    }
    match agent.allow_hosts(
        std::slice::from_ref(&host),
        caller(&headers).as_deref(),
        None,
        None,
    ) {
        Ok(_) => Json(json!({"host": host})).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

async fn remove_host(State(agent): App, Path(host): Path<String>, headers: HeaderMap) -> Response {
    match agent.remove_host(&host.to_ascii_lowercase(), caller(&headers).as_deref()) {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "that site isn't on the list"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// The saved keys, by name and label. Values never leave the desktop.
/// Notifications (notify.rs): the key browsers subscribe with, and the
/// browsers where they're on.
async fn push_info(State(agent): App) -> Response {
    match agent.push_info() {
        Ok(info) => Json(info).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

#[derive(Deserialize)]
struct SubscriptionKeys {
    p256dh: String,
    auth: String,
}

/// A browser's `PushSubscription.toJSON()`, and what to call it.
#[derive(Deserialize)]
struct NewSubscription {
    endpoint: String,
    keys: SubscriptionKeys,
    #[serde(default)]
    label: String,
}

async fn subscribe_push(State(agent): App, Json(body): Json<NewSubscription>) -> Response {
    match agent.subscribe_push(&body.endpoint, &body.keys.p256dh, &body.keys.auth, &body.label) {
        Ok(id) => Json(json!({"id": id})).into_response(),
        Err(crate::notify::PushError::Invalid(problem)) => error(StatusCode::BAD_REQUEST, &problem),
        Err(crate::notify::PushError::Failed(err)) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}"))
        }
    }
}

async fn unsubscribe_push(State(agent): App, Path(id): Path<String>) -> Response {
    match agent.unsubscribe_push(&id) {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "no such browser"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// An iPhone or Mac app's APNs registration: its device token, its bundle id
/// (the topic) and which APNs servers the token is for.
#[derive(Deserialize)]
struct NewDevice {
    token: String,
    topic: String,
    #[serde(default)]
    environment: String,
    /// What to call the device in the list (the app sends its name).
    #[serde(default)]
    name: String,
}

async fn register_device(State(agent): App, Json(body): Json<NewDevice>) -> Response {
    match agent.register_device(&body.token, &body.topic, &body.environment, &body.name) {
        Ok(id) => Json(json!({"id": id})).into_response(),
        Err(crate::notify::PushError::Invalid(problem)) => error(StatusCode::BAD_REQUEST, &problem),
        Err(crate::notify::PushError::Failed(err)) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}"))
        }
    }
}

async fn unregister_device(State(agent): App, Path(id): Path<String>) -> Response {
    match agent.unregister_device(&id) {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "no such device"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize, Default)]
struct PushTest {
    #[serde(default)]
    id: Option<String>,
}

/// A test notification, to one browser or all of them.
async fn test_push(State(agent): App, body: Option<Json<PushTest>>) -> Response {
    let id = body.and_then(|Json(b)| b.id);
    let notice = crate::notify::Notice::plain(
        "Notifications work".into(),
        "You'll hear from your agent here when it needs you or finishes something.".into(),
        "test".into(),
        "normal",
        600,
    );
    match agent.notify(&notice, id.as_deref()) {
        Ok(0) if id.is_some() => error(StatusCode::NOT_FOUND, "no such browser"),
        Ok(queued) => Json(json!({"queued": queued})).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn model_keys(State(agent): App) -> Json<Value> {
    Json(json!({"providers": agent.model_providers()}))
}

#[derive(Deserialize)]
struct NewModelKey {
    value: String,
}

/// Saves a provider's key from the app (pasted into a secure field), once
/// the provider takes it: it's used from the next model call on, with no
/// restart. `checked` false: the provider couldn't be asked, and it was saved
/// anyway. The body is never logged.
async fn save_model_key(
    State(agent): App,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Json(body): Json<NewModelKey>,
) -> Response {
    let Some(provider) = llm::Provider::parse(&provider) else {
        return error(StatusCode::NOT_FOUND, "no such provider (anthropic or together)");
    };
    match agent
        .save_model_key(provider, &body.value, caller(&headers))
        .await
    {
        Ok(checked) => {
            Json(json!({"checked": checked, "providers": agent.model_providers()})).into_response()
        }
        Err(SecretError::Invalid(problem)) => error(StatusCode::BAD_REQUEST, &problem),
        Err(SecretError::Failed(err)) => {
            tracing::error!(%err, provider = provider.id(), "couldn't save a model key");
            error(StatusCode::INTERNAL_SERVER_ERROR, "couldn't save the key")
        }
    }
}

/// Removes the key pasted in the app (the deployment's, if any, takes over).
async fn delete_model_key(State(agent): App, Path(provider): Path<String>, headers: HeaderMap) -> Response {
    let Some(provider) = llm::Provider::parse(&provider) else {
        return error(StatusCode::NOT_FOUND, "no such provider (anthropic or together)");
    };
    match agent.remove_model_key(provider, caller(&headers)) {
        Ok(true) => Json(json!({"providers": agent.model_providers()})).into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "no key for it was saved in the app"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

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

async fn image(State(agent): App, Path(id): Path<String>) -> Response {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return error(StatusCode::BAD_REQUEST, "bad image id");
    }
    match tokio::fs::read(agent.cfg.state_dir.join("images").join(format!("{id}.jpg"))).await {
        // Chrome's screenshots are JPEGs; the desktop's (HCU's) are PNGs, saved
        // under the same name, so the bytes say which.
        Ok(bytes) => {
            let png = bytes.starts_with(b"\x89PNG\r\n\x1a\n");
            let kind = if png { "image/png" } else { "image/jpeg" };
            ([(header::CONTENT_TYPE, kind)], bytes).into_response()
        }
        Err(_) => error(StatusCode::NOT_FOUND, "no such image"),
    }
}

async fn settings(State(agent): App) -> Json<Value> {
    Json(json!(agent.settings()))
}

#[derive(Deserialize)]
struct SettingsUpdate {
    model: Option<String>,
    allow_actions: Option<bool>,
    auto_approve: Option<bool>,
    memory_consolidation: Option<String>,
    memory_consolidation_model: Option<String>,
    memory_consolidation_daily_cap_usd: Option<f64>,
    /// A number sets the monthly budget; null removes it.
    #[serde(default, deserialize_with = "double_option")]
    monthly_budget_usd: Option<Option<f64>>,
}

async fn put_settings(State(agent): App, headers: HeaderMap, Json(body): Json<SettingsUpdate>) -> Response {
    if let Some(budget) = body.monthly_budget_usd {
        match agent.set_budget(budget) {
            Ok(Err(problem)) => return error(StatusCode::BAD_REQUEST, &problem),
            Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
            Ok(Ok(_)) => {}
        }
    }
    // Older clients sent the entire settings object. Their explicit turn-off
    // gesture still works. No generic update can turn Auto approve on. When
    // it's already off, nothing changes: the version a newer app holds for
    // its confirmation stays valid, and no change is announced.
    if body.auto_approve == Some(false)
        && agent.settings().auto_approve
        && let Err(err) = agent.set_auto_approve(false, None, caller(&headers).as_deref())
    {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    let has_memory = body.memory_consolidation.is_some()
        || body.memory_consolidation_model.is_some()
        || body.memory_consolidation_daily_cap_usd.is_some();
    if has_memory {
        match agent.update_memory_settings(
            body.memory_consolidation.as_deref(),
            body.memory_consolidation_model.as_deref(),
            body.memory_consolidation_daily_cap_usd,
        ) {
            Ok(Err(problem)) => return error(StatusCode::BAD_REQUEST, &problem),
            Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
            Ok(Ok(_)) => {}
        }
    }
    if body.model.is_some() || body.allow_actions.is_some() {
        // Older clients include the whole settings object on unrelated edits.
        return match agent.update_settings(body.model.as_deref(), body.allow_actions) {
            Ok(Ok(settings)) => Json(json!(settings)).into_response(),
            Ok(Err(problem)) => error(StatusCode::BAD_REQUEST, &problem),
            Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        };
    }
    if !has_memory && body.monthly_budget_usd.is_none() && body.auto_approve == Some(true) {
        return error(
            StatusCode::CONFLICT,
            "Use the Auto approve confirmation endpoint to turn it on",
        );
    }
    Json(json!(agent.settings())).into_response()
}

#[derive(Deserialize)]
struct AutoApproveUpdate {
    enabled: bool,
    expected_version: Option<String>,
}

async fn set_auto_approve(
    State(agent): App,
    headers: HeaderMap,
    Json(body): Json<AutoApproveUpdate>,
) -> Response {
    match agent.set_auto_approve(
        body.enabled,
        body.expected_version.as_deref(),
        caller(&headers).as_deref(),
    ) {
        Ok(Some(settings)) => Json(json!(settings)).into_response(),
        Ok(None) => error(
            StatusCode::CONFLICT,
            "Auto approve changed. Review the current setting and confirm again.",
        ),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_approve_needs_current_confirmation_and_ignores_whole_legacy_settings() {
        let (agent, dir) = crate::agent::test_agent();
        let update = |body: Value| {
            let settings = serde_json::from_value(body).unwrap();
            put_settings(State(agent.clone()), HeaderMap::new(), Json(settings))
        };
        let confirm = |enabled, expected_version| {
            set_auto_approve(
                State(agent.clone()),
                HeaderMap::new(),
                Json(AutoApproveUpdate {
                    enabled,
                    expected_version,
                }),
            )
        };
        let initial = agent.settings().auto_approve_version;
        assert_eq!(confirm(true, None).await.status(), StatusCode::CONFLICT);
        assert_eq!(
            update(json!({"auto_approve": true})).await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(confirm(false, None).await.status(), StatusCode::OK);
        assert_eq!(confirm(true, Some(initial)).await.status(), StatusCode::CONFLICT);
        let current = agent.settings().auto_approve_version;
        assert_eq!(
            confirm(true, Some(current.clone())).await.status(),
            StatusCode::OK
        );
        assert!(agent.settings().auto_approve);
        assert_eq!(confirm(true, Some(current)).await.status(), StatusCode::CONFLICT);
        assert_eq!(
            update(json!({"model": "zai-org/GLM-5.3-Flash", "auto_approve": false}))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            update(json!({"model": "moonshotai/Kimi-K3", "allow_actions": false, "auto_approve": true}))
                .await
                .status(),
            StatusCode::OK
        );
        let settings = agent.settings();
        assert_eq!(settings.model, "moonshotai/Kimi-K3");
        assert!(!settings.allow_actions);
        assert!(!settings.auto_approve);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_legacy_turn_off_when_already_off_changes_nothing() {
        let (agent, dir) = crate::agent::test_agent();
        let update = |body: Value| {
            let settings = serde_json::from_value(body).unwrap();
            put_settings(State(agent.clone()), HeaderMap::new(), Json(settings))
        };
        let changes = || {
            agent
                .store
                .events_after(0, 10_000)
                .unwrap()
                .iter()
                .filter(|event| event.kind == "auto_approve.changed")
                .count()
        };
        let version = agent.settings().auto_approve_version;
        assert_eq!(
            update(json!({"model": "zai-org/GLM-5.3", "allow_actions": true, "auto_approve": false}))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            update(json!({"auto_approve": false})).await.status(),
            StatusCode::OK
        );
        assert_eq!(agent.settings().auto_approve_version, version);
        assert_eq!(changes(), 0);
        // A newer app's confirmation against that version still applies.
        let confirmed = set_auto_approve(
            State(agent.clone()),
            HeaderMap::new(),
            Json(AutoApproveUpdate {
                enabled: true,
                expected_version: Some(version),
            }),
        )
        .await;
        assert_eq!(confirmed.status(), StatusCode::OK);
        // On, the legacy turn-off still turns it off.
        assert_eq!(
            update(json!({"auto_approve": false})).await.status(),
            StatusCode::OK
        );
        assert!(!agent.settings().auto_approve);
        assert_eq!(changes(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn restoring_at_the_memory_cap_is_a_conflict_not_a_missing_memory() {
        let (agent, dir) = crate::agent::test_agent();
        let expired: i64 = agent
            .store
            .transact(|tx| {
                let id = tx.query_row(
                    "INSERT INTO memory (text, trusted, trust, status)
                     VALUES ('Old note', 0, 'agent_inferred', 'expired') RETURNING id",
                    [],
                    |r| r.get(0),
                )?;
                for i in 0..memory::MAX_ACTIVE {
                    tx.execute(
                        "INSERT INTO memory (text, trusted, trust) VALUES (?1, 0, 'agent_inferred')",
                        rusqlite::params![format!("filler {i}")],
                    )?;
                }
                Ok(id)
            })
            .unwrap();
        let restore = |id: i64| restore_memory(State(agent.clone()), Path(id), HeaderMap::new());
        let full = restore(expired).await;
        assert_eq!(full.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(full.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("too many active memories"));
        assert_eq!(restore(expired + 100_000).await.status(), StatusCode::NOT_FOUND);
        // With room again, it comes back.
        let filler: i64 = agent
            .store
            .read(|conn| {
                Ok(conn.query_row("SELECT id FROM memory WHERE text = 'filler 0'", [], |r| r.get(0))?)
            })
            .unwrap();
        memory::forget(&agent.store, memory::Actor::User { login: None }, filler).unwrap();
        assert_eq!(restore(expired).await.status(), StatusCode::OK);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn event_cursor_replays_a_long_filtered_backlog_and_recovers_when_broadcast_lags() {
        let (agent, dir) = crate::agent::test_agent();
        for _ in 0..5_100 {
            agent.store.append_event("step", &Value::Null).unwrap();
        }
        let first = agent.emit("message", Value::Null).unwrap();
        let mut cursor = EventCursor::new(agent.clone(), 0, vec!["message".into()]);
        assert_eq!(cursor.next().await.unwrap().seq, first);

        let third = {
            let next = cursor.next();
            tokio::pin!(next);
            let mut cx = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
            assert!(std::future::Future::poll(next.as_mut(), &mut cx).is_pending());
            let second = agent.emit("message", Value::Null).unwrap();
            for _ in 0..1_100 {
                agent.emit("step", Value::Null).unwrap();
            }
            let third = agent.emit("message", Value::Null).unwrap();
            assert_eq!(next.await.unwrap().seq, second);
            third
        };
        assert_eq!(cursor.next().await.unwrap().seq, third);
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn body(reply: Response) -> Value {
        let bytes = axum::body::to_bytes(reply.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn message(text: &str) -> Json<NewMessage> {
        Json(serde_json::from_value(json!({"text": text})).unwrap())
    }

    #[tokio::test]
    async fn a_message_for_a_busy_conversation_is_queued_not_refused() {
        let (agent, dir) = crate::agent::test_agent();
        // `main` waits on a card.
        agent
            .store
            .start_run("t1", "do it", None, "m", &agent.policy_version(), None)
            .unwrap();
        agent
            .store
            .park(
                "a1",
                "t1",
                &json!({"kind": "shell", "command": "ls"}),
                "{}",
                &agent.policy_version(),
            )
            .unwrap();
        let reply = post_message(State(agent.clone()), HeaderMap::new(), message("next")).await;
        assert_eq!(reply.status(), StatusCode::ACCEPTED);
        assert_eq!(body(reply).await["queued"]["position"], 1);
        let reply = post_session_message(
            State(agent.clone()),
            Path(MAIN.into()),
            HeaderMap::new(),
            message("and this"),
        )
        .await;
        assert_eq!(reply.status(), StatusCode::ACCEPTED);
        let queued = body(reply).await;
        assert_eq!(queued["queued"]["position"], 2);
        let unknown = post_session_message(
            State(agent.clone()),
            Path("nope".into()),
            HeaderMap::new(),
            message("hi"),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        let empty = post_message(State(agent.clone()), HeaderMap::new(), message("  ")).await;
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
        // The ✕ on one, then Stop: the old shape, plus what was dropped.
        let id = queued["queued"]["id"].as_str().unwrap().to_string();
        let removed = unqueue(State(agent.clone()), Path((MAIN.into(), id.clone()))).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let again = unqueue(State(agent.clone()), Path((MAIN.into(), id))).await;
        assert_eq!(again.status(), StatusCode::NOT_FOUND);
        let Json(stopped) = stop(State(agent.clone())).await;
        // A card given up on: something stopped, no running task.
        assert_eq!(stopped, json!({"stopping": false, "stopped": true, "dropped": 1}));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn conversations_usage_and_the_budget_through_the_api() {
        let (agent, dir) = crate::agent::test_agent();
        let made = create_session(State(agent.clone()), None).await;
        assert_eq!(made.status(), StatusCode::CREATED);
        let id = body(made).await["id"].as_str().unwrap().to_string();
        assert_eq!(
            get_session(State(agent.clone()), Path(id.clone())).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            get_session(State(agent.clone()), Path("nope".into()))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        let change = SessionChange {
            title: Some("Lisbon".into()),
            archived: None,
        };
        let renamed = update_session(State(agent.clone()), Path(id.clone()), Json(change)).await;
        assert_eq!(body(renamed).await["title"], "Lisbon");
        let listed =
            body(list_sessions(State(agent.clone()), Query(ArchivedQuery { archived: None })).await).await;
        assert_eq!(listed["running_limit"], 3);
        assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(
            listed["sessions"][0]["id"],
            id.as_str(),
            "most recently updated first"
        );
        let stopped = body(stop_session(State(agent.clone()), Path(id.clone())).await).await;
        assert_eq!(
            stopped,
            json!({"stopping": false, "stopped": false, "dropped": 0})
        );
        assert_eq!(
            delete_session(State(agent.clone()), Path(MAIN.into()))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            delete_session(State(agent.clone()), Path(id.clone()))
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            delete_session(State(agent.clone()), Path(id.clone()))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        for gone in [
            session_messages(
                State(agent.clone()),
                Path(id.clone()),
                Query(Limit { limit: None }),
            )
            .await,
            stop_session(State(agent.clone()), Path(id.clone())).await,
        ] {
            assert_eq!(gone.status(), StatusCode::NOT_FOUND);
        }
        // Usage, a week of it.
        let usage = body(usage(State(agent.clone()), Query(UsageQuery { days: Some(7) })).await).await;
        assert_eq!(usage["daily"].as_array().unwrap().len(), 7);
        assert_eq!(usage["currency"], "USD");
        // The budget: a positive number up to 100,000, or null to remove it.
        let set = |value: Value| {
            let settings = serde_json::from_value(json!({"monthly_budget_usd": value})).unwrap();
            put_settings(State(agent.clone()), HeaderMap::new(), Json(settings))
        };
        for bad in [json!(-1), json!(0), json!(100_001)] {
            assert_eq!(set(bad.clone()).await.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
        assert_eq!(body(set(json!(50)).await).await["monthly_budget_usd"], 50.0);
        let Json(status) = status(State(agent.clone())).await;
        assert_eq!(status["budget"]["monthly_usd"], 50.0);
        assert_eq!(status["budget"]["paused"], false);
        assert_eq!(
            status["sessions"],
            json!({"working": 0, "waiting": 0, "queued": 0, "running_limit": 3})
        );
        assert_eq!(
            body(set(json!(null)).await).await["monthly_budget_usd"],
            Value::Null
        );
        // Older apps' whole settings object leaves it alone.
        agent.set_budget(Some(20.0)).unwrap().unwrap();
        let settings = serde_json::from_value(json!({"model": "m", "allow_actions": true})).unwrap();
        put_settings(State(agent.clone()), HeaderMap::new(), Json(settings)).await;
        assert_eq!(agent.settings().monthly_budget_usd, Some(20.0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_schedule_from_the_app_runs_without_asking_unless_told_otherwise() {
        let body = |extra: &str| {
            serde_json::from_str::<NewSchedule>(&format!(
                r#"{{"name":"n","prompt":"p","cron":"0 9 * * *"{extra}}}"#
            ))
            .unwrap()
            .auto_approve
        };
        assert!(body(""));
        assert!(!body(r#","auto_approve":false"#));
    }

    #[test]
    fn the_caller_comes_from_tailnet_proxy() {
        let mut headers = HeaderMap::new();
        assert_eq!(caller(&headers), None);
        // Only tailnet-proxy's header counts.
        headers.insert("x-agent-user", "me@acme.com".parse().unwrap());
        assert_eq!(caller(&headers), None);
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
                with(|f| f.latest = Some(("t1".into(), status.into(), "main".into()))),
                "problem",
                "{status}"
            );
        }
        assert_eq!(
            with(|f| f.latest = Some(("t1".into(), "done".into(), "main".into()))),
            "idle"
        );
        assert_eq!(
            with(|f| f.working = vec![("main".into(), "t1".into())]),
            "working"
        );
        assert_eq!(
            with(|f| f.latest = Some(("t1".into(), "needs_user".into(), "main".into()))),
            "needs_you/action"
        );
        // An open card in a conversation that isn't the latest's.
        assert_eq!(
            with(|f| f.actions = vec![("t0".into(), "c2".into())]),
            "needs_you/action"
        );
        assert_eq!(
            with(|f| f.cards = vec![("s1".into(), "secret".into(), "main".into())]),
            "needs_you/secret"
        );
        for kind in [
            "shell", "browser", "app", "install", "schedule", "access", "stuck",
        ] {
            assert_eq!(
                with(|f| f.cards = vec![("a1".into(), kind.into(), "main".into())]),
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
            cards: vec![
                ("s1".into(), "secret".into(), "main".into()),
                ("a1".into(), "browser".into(), "c2".into()),
            ],
            actions: vec![],
            latest: Some(("t9".into(), "needs_user".into(), "main".into())),
            working: vec![("c3".into(), "t8".into())],
            failing_effects: 3,
            next_schedule: Some((4, "Morning inbox".into(), 100)),
        };
        // A card in any conversation, with the conversation to open.
        assert_eq!(
            stage_of(&everything),
            (
                "needs_you/approval",
                json!({"card": "a1", "kind": "browser", "session": "c2"})
            )
        );
        let working = StageFacts {
            cards: vec![],
            latest: Some(("t9".into(), "error".into(), "main".into())),
            ..everything
        };
        assert_eq!(
            stage_of(&working),
            ("working", json!({"run": "t8", "session": "c3"}))
        );
        let problem = StageFacts {
            working: vec![],
            ..working
        };
        assert_eq!(
            stage_of(&problem),
            (
                "problem",
                json!({"run": "t9", "status": "error", "session": "main"})
            )
        );
    }
}
