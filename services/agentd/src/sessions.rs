//! Conversations ("sessions"): several chats, each with its own history,
//! browser window and cards, sharing memory, keys, schedules and settings.
//! `main` holds everything from before them and can't be deleted.
//!
//! A message starts a task at once only when its conversation is free (not
//! working, not waiting on a card, nothing queued before it), fewer than
//! `MAX_PARALLEL_TASKS` conversations are working, and the month's budget
//! isn't used up. Otherwise it waits in `chat_queue`, which outlives
//! restarts. `pump` starts queued messages, oldest first and one per
//! conversation, whenever a slot frees.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::agent::{Agent, Lane, Posted, short_id};
use crate::memory::unix_now;
use crate::store::{MAIN, SessionRow};

/// A conversation's title: at most this many characters.
const TITLE_CHARS: usize = 60;
/// `last.text`: the start of its newest history entry.
const LAST_CHARS: usize = 160;

/// A title from a message: its first line, whitespace collapsed, at most 60
/// characters, cut at a word with "…". No model call.
pub fn title_from(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let flat = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= TITLE_CHARS {
        return flat;
    }
    // Room for the "…"; back to the start of a word cut in two.
    let mut end = TITLE_CHARS - 1;
    if chars[end] != ' '
        && let Some(space) = chars[..end].iter().rposition(|c| *c == ' ')
        && space > 0
    {
        end = space;
    }
    let kept: String = chars[..end].iter().collect();
    format!("{}…", kept.trim_end())
}

/// Why a message wasn't taken.
#[derive(Debug)]
pub enum PostError {
    /// No such conversation, or it's archived or deleted.
    NotFound,
    Failed(anyhow::Error),
}

/// What Stop did.
#[derive(Debug, PartialEq)]
pub struct Stopped {
    /// A running task was told to stop (`task.stopping`; `task.finished` follows).
    pub stopping: bool,
    /// Anything was: a running task, or a card given up on.
    pub stopped: bool,
    /// Queued messages dropped.
    pub dropped: usize,
}

impl Stopped {
    /// The API's answer: `{stopping, stopped, dropped}`.
    pub fn json(&self) -> Value {
        json!({"stopping": self.stopping, "stopped": self.stopped, "dropped": self.dropped})
    }
}

/// Why a conversation couldn't be changed.
#[derive(Debug)]
pub enum SessionError {
    NotFound,
    /// Working or waiting on a card, or `main` (which can't be deleted).
    Conflict(&'static str),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for SessionError {
    fn from(err: anyhow::Error) -> Self {
        Self::Failed(err)
    }
}

/// What the conversations' states are worked out from, read once for a list.
struct Facts {
    running: HashSet<String>,
    /// The newest waiting card's kind, by conversation.
    cards: HashMap<String, String>,
    /// Conversations whose latest task ended on Action needed, card open.
    actions: HashSet<String>,
    paused: bool,
}

impl Agent {
    /// A message for a conversation: starts its task, or queues it (§2.1 of
    /// docs/conversations-and-usage.md).
    pub fn post(
        self: &Arc<Self>,
        session: &str,
        text: String,
        by: Option<String>,
    ) -> Result<Posted, PostError> {
        self.post_to(session, text, by, None)
    }

    /// `post`; `tab`: the window it carries on in ("I'm done" after a
    /// scheduled run kept one), which becomes the conversation's when it
    /// starts.
    pub(crate) fn post_to(
        self: &Arc<Self>,
        session: &str,
        text: String,
        by: Option<String>,
        tab: Option<String>,
    ) -> Result<Posted, PostError> {
        let row = match self.store.session(session) {
            Ok(Some(row)) if row.deleted.is_none() && !row.archived => row,
            Ok(_) => return Err(PostError::NotFound),
            Err(err) => return Err(PostError::Failed(err)),
        };
        if row.title.is_empty() {
            self.retitle(session, &title_from(&self.secrets.redact(&text)));
        }
        // A card nobody answered for a day can't be resumed: it mustn't hold
        // this message back.
        self.expire_stale_cards(Some(session));
        // A memory command isn't a task for the model: it costs nothing and
        // leaves a card waiting, so neither the limit nor the budget holds
        // it back. It still waits behind a running task or queued messages.
        let memory_command = self.is_memory_command(&text, by.as_deref());
        let paused = self.budget_paused();
        let now = unix_now();
        let mut lanes = self.lanes.lock().unwrap();
        // Again under the lanes' lock, which archiving and deleting hold:
        // no task starts in a conversation going away.
        match self.store.session(session) {
            Ok(Some(row)) if row.deleted.is_none() && !row.archived => {}
            Ok(_) => return Err(PostError::NotFound),
            Err(err) => return Err(PostError::Failed(err)),
        }
        let working = lanes.values().filter(|lane| lane.running.is_some()).count();
        let lane = lanes.entry(session.to_string()).or_insert_with(Lane::new);
        let waiting = self.store.waiting_in(session).map_err(PostError::Failed)?;
        let queued = self.store.queued_in(session).map_err(PostError::Failed)?.len();
        let start = lane.running.is_none()
            && queued == 0
            && (memory_command || (!waiting && working < self.cfg.max_parallel_tasks && !paused));
        if start {
            let task = short_id();
            lane.running = Some(task.clone());
            lane.cancel.send_replace(false);
            drop(lanes);
            if let Some(tab) = &tab {
                self.adopt_window(session, tab);
            }
            self.spawn_task(task.clone(), text, by, session.to_string());
            return Ok(Posted::Started(task));
        }
        let id = short_id();
        let position = self
            .store
            .queue_message(&id, session, &text, by.as_deref(), tab.as_deref(), now)
            .map_err(PostError::Failed)?;
        drop(lanes);
        tracing::info!(%session, queued = %id, position, "message queued");
        self.emit(
            "message.queued",
            json!({"session": session, "id": id, "text": text, "by": by, "created": now, "position": position}),
        );
        Ok(Posted::Queued { id, position })
    }

    /// Starts queued messages while there's room: oldest first, one per
    /// conversation, never in one that's working or waiting on a card, and
    /// not while the budget is used up. Called whenever a slot may have
    /// freed: a task ends or parks, Stop, the budget changes, the month
    /// turns over (the scheduler's minute), agentd starts.
    pub fn pump(self: &Arc<Self>) {
        loop {
            if self.budget_paused() {
                return;
            }
            let (next, task) = {
                let mut lanes = self.lanes.lock().unwrap();
                let working = lanes.values().filter(|lane| lane.running.is_some()).count();
                if working >= self.cfg.max_parallel_tasks {
                    return;
                }
                let (Ok(heads), Ok(waiting)) = (self.store.queue_heads(), self.store.waiting_sessions())
                else {
                    tracing::error!("couldn't read the queued messages");
                    return;
                };
                let free = |session: &str| {
                    !waiting.contains(session) && lanes.get(session).is_none_or(|lane| lane.running.is_none())
                };
                let Some(next) = heads.into_iter().find(|queued| free(&queued.session)) else {
                    return;
                };
                match self.store.take_queued(&next.id) {
                    Ok(true) => {}
                    Ok(false) => continue, // removed meanwhile
                    Err(err) => {
                        tracing::error!(%err, "couldn't take a queued message");
                        return;
                    }
                }
                let task = short_id();
                let lane = lanes.entry(next.session.clone()).or_insert_with(Lane::new);
                lane.running = Some(task.clone());
                lane.cancel.send_replace(false);
                (next, task)
            };
            tracing::info!(session = %next.session, queued = %next.id, %task, "queued message started");
            self.emit(
                "message.unqueued",
                json!({"session": next.session, "id": next.id, "reason": "started", "task": task}),
            );
            if let Some(tab) = &next.tab {
                self.adopt_window(&next.session, tab);
            }
            self.spawn_task(task, next.text, next.by, next.session);
        }
    }

    /// Stop in a conversation: stops its running task, or gives up on the
    /// one parked on a card, and drops its queued messages.
    pub fn stop(self: &Arc<Self>, session: &str) -> Stopped {
        // First, so the end of the task doesn't start the next one.
        let dropped = self.drop_queue(session);
        let running = match self.lanes.lock().unwrap().get(session) {
            Some(lane) if lane.running.is_some() => {
                // The model call, browser action or command in progress is
                // interrupted.
                lane.cancel.send_replace(true);
                true
            }
            _ => false,
        };
        if running {
            // Said at once; task.finished follows.
            self.emit("task.stopping", json!({"session": session}));
            return Stopped {
                stopping: true,
                stopped: true,
                dropped,
            };
        }
        // Waiting for an approval: stopping it means giving up on it.
        let stopped = self.abandon_parked(session, "Stopped.");
        // An answer that won the race resumed the task meanwhile: Stop wins.
        let resumed = match self.lanes.lock().unwrap().get(session) {
            Some(lane) if lane.running.is_some() => {
                lane.cancel.send_replace(true);
                true
            }
            _ => false,
        };
        if resumed {
            self.emit("task.stopping", json!({"session": session}));
            return Stopped {
                stopping: true,
                stopped: true,
                dropped,
            };
        }
        self.pump();
        Stopped {
            stopping: false,
            stopped,
            dropped,
        }
    }

    /// Drops a conversation's queued messages; how many.
    fn drop_queue(&self, session: &str) -> usize {
        let ids = self.store.drop_queue(session).unwrap_or_else(|err| {
            tracing::error!(%err, %session, "couldn't drop the queued messages");
            Vec::new()
        });
        for id in &ids {
            self.emit(
                "message.unqueued",
                json!({"session": session, "id": id, "reason": "stopped"}),
            );
        }
        ids.len()
    }

    /// The app's ✕ on a queued message. False if it's gone (started or
    /// removed already).
    pub fn unqueue(&self, session: &str, id: &str) -> Result<bool> {
        if !self.store.remove_queued(session, id)? {
            return Ok(false);
        }
        self.emit(
            "message.unqueued",
            json!({"session": session, "id": id, "reason": "removed"}),
        );
        Ok(true)
    }

    /// Sets a conversation's title and says so.
    fn retitle(&self, session: &str, title: &str) {
        if title.is_empty() {
            return;
        }
        if let Err(err) = self.store.set_session_title(session, title, unix_now()) {
            tracing::error!(%err, %session, "couldn't set the conversation's title");
            return;
        }
        if let Some(view) = self.session_view(session) {
            self.emit("session.updated", view);
        }
    }

    fn facts(&self) -> Facts {
        let mut cards = HashMap::new();
        for (_, kind, session) in self.store.parked_cards().unwrap_or_default() {
            // Newest first: the newest card's kind stays.
            cards.entry(session).or_insert(kind);
        }
        Facts {
            running: self.working().into_iter().map(|(session, _)| session).collect(),
            cards,
            actions: self
                .store
                .open_actions()
                .unwrap_or_default()
                .into_iter()
                .map(|(_, session)| session)
                .collect(),
            paused: self.budget_paused(),
        }
    }

    /// A conversation as the apps list it (`Session`).
    fn describe(&self, row: &SessionRow, facts: &Facts) -> Value {
        let queued: Vec<Value> = self
            .store
            .queued_in(&row.id)
            .unwrap_or_default()
            .into_iter()
            .map(|mut queued| {
                queued.text = self.secrets.redact(&queued.text).into_owned();
                json!(queued)
            })
            .collect();
        let state = if facts.running.contains(&row.id) {
            "working"
        } else if facts.cards.contains_key(&row.id) {
            "waiting"
        } else if !queued.is_empty() && facts.paused {
            "paused"
        } else if !queued.is_empty() {
            "queued"
        } else {
            "idle"
        };
        let needs_you = match facts.cards.get(&row.id) {
            Some(kind) if kind == "secret" => Some("secret"),
            Some(_) => Some("approval"),
            None if facts.actions.contains(&row.id) => Some("action"),
            None => None,
        };
        let last = self
            .store
            .last_message(&row.id)
            .ok()
            .flatten()
            .map(|(role, text, ts)| {
                let text = self.secrets.redact(&text);
                let start: String = text.chars().take(LAST_CHARS).collect();
                let text = if start.len() < text.len() {
                    format!("{start}…")
                } else {
                    start
                };
                json!({"role": role, "text": text, "ts": ts})
            });
        // A key saved after the title was made doesn't show in it.
        json!({
            "id": row.id, "title": self.secrets.redact(&row.title), "created": row.created, "updated": row.updated,
            "archived": row.archived, "state": state, "needs_you": needs_you, "queued": queued,
            "last": last,
        })
    }

    /// A conversation, unless it's unknown or deleted.
    pub fn session_view(&self, id: &str) -> Option<Value> {
        let row = self.store.session(id).ok().flatten()?;
        if row.deleted.is_some() {
            return None;
        }
        Some(self.describe(&row, &self.facts()))
    }

    /// The archived conversations or the others, most recently updated first.
    pub fn sessions_view(&self, archived: bool) -> Result<Vec<Value>> {
        let facts = self.facts();
        Ok(self
            .store
            .sessions(archived)?
            .iter()
            .map(|row| self.describe(row, &facts))
            .collect())
    }

    /// A new, empty conversation ("New conversation" in the apps).
    pub fn create_session(&self, title: Option<&str>) -> Result<Value> {
        let id = short_id();
        let title = title.map(title_from).unwrap_or_default();
        let row = self.store.create_session(&id, &title, unix_now())?;
        let view = self.describe(&row, &self.facts());
        tracing::info!(session = %id, "conversation created");
        self.emit("session.created", view.clone());
        Ok(view)
    }

    /// Renames, archives or brings back a conversation. Archiving one that's
    /// working or waiting on a card is refused; archiving closes its windows.
    pub fn update_session(
        self: &Arc<Self>,
        id: &str,
        title: Option<&str>,
        archived: Option<bool>,
    ) -> Result<Value, SessionError> {
        let row = match self.store.session(id)? {
            Some(row) if row.deleted.is_none() => row,
            _ => return Err(SessionError::NotFound),
        };
        let change = archived.filter(|archived| *archived != row.archived);
        {
            // The check and the change under the lanes' lock, which a
            // message starting a task takes too.
            let lanes = self.lanes.lock().unwrap();
            if change == Some(true) && self.busy_in(&lanes, id)? {
                return Err(SessionError::Conflict(
                    "it's working or waiting on a card: stop it first",
                ));
            }
            if let Some(title) = title {
                self.store.set_session_title(id, &title_from(title), unix_now())?;
            }
            if let Some(archived) = change {
                self.store.set_session_archived(id, archived)?;
                if archived {
                    self.close_windows(&row);
                }
            }
        }
        if change == Some(false) {
            // Its queued messages can go again.
            self.pump();
        }
        let view = self.session_view(id).ok_or(SessionError::NotFound)?;
        self.emit("session.updated", view.clone());
        Ok(view)
    }

    /// Deletes a conversation (never `main`): its history, queued messages
    /// and open Action needed cards go; its runs and events stay for the
    /// audit log and usage.
    pub fn delete_session(&self, id: &str) -> Result<(), SessionError> {
        if id == MAIN {
            return Err(SessionError::Conflict("the first conversation can't be deleted"));
        }
        let row = match self.store.session(id)? {
            Some(row) if row.deleted.is_none() => row,
            _ => return Err(SessionError::NotFound),
        };
        {
            // As archiving: no task starts in it meanwhile.
            let mut lanes = self.lanes.lock().unwrap();
            if self.busy_in(&lanes, id)? {
                return Err(SessionError::Conflict(
                    "it's working or waiting on a card: stop it first",
                ));
            }
            // Before its cards go with it.
            self.close_windows(&row);
            self.store.delete_session(id, unix_now())?;
            lanes.remove(id);
        }
        tracing::info!(session = %id, "conversation deleted");
        self.emit("session.deleted", json!({"id": id}));
        Ok(())
    }

    /// Working, or waiting on a card, with the lanes' lock held.
    fn busy_in(&self, lanes: &HashMap<String, Lane>, id: &str) -> Result<bool> {
        Ok(lanes.get(id).is_some_and(|lane| lane.running.is_some()) || self.store.waiting_in(id)?)
    }

    /// Closes a conversation's windows: its own (`main` has none: it works
    /// in the user's tab), and those its Action needed cards keep.
    fn close_windows(&self, row: &SessionRow) {
        let mut tabs: Vec<String> = self
            .store
            .action_tabs()
            .unwrap_or_default()
            .into_iter()
            .filter(|(session, _)| *session == row.id)
            .map(|(_, tab)| tab)
            .collect();
        if row.id != MAIN
            && let Some(tab) = row.tab_target.clone()
        {
            if let Err(err) = self.store.set_session_tab(&row.id, None) {
                tracing::warn!(%err, session = %row.id, "couldn't forget the conversation's window");
            }
            tabs.push(tab);
        }
        for tab in tabs {
            self.close_tab_window(tab);
        }
    }

    /// For `/status`: how many conversations work and wait, how many
    /// messages are queued, and the limit.
    pub fn sessions_status(&self) -> Value {
        json!({
            "working": self.working().len(),
            "waiting": self.store.waiting_sessions().map_or(0, |w| w.len()),
            "queued": self.store.queued_messages().unwrap_or(0),
            "running_limit": self.cfg.max_parallel_tasks,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use axum::Router;
    use axum::routing::post;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::agent::StartError;
    use crate::llm::Usage;

    /// A stand-in model: a request whose last user message says "hold"
    /// waits for a permit (one per answer, in the order they came);
    /// anything else is answered at once with "Done: <message>". It keeps
    /// every request.
    async fn gated_model() -> (String, Arc<Semaphore>, Arc<Mutex<Vec<Value>>>) {
        let gate = Arc::new(Semaphore::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (held, log) = (gate.clone(), seen.clone());
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let (held, log) = (held.clone(), log.clone());
                async move {
                    let asked = body["messages"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .rfind(|m| m["role"] == "user")
                        .and_then(|m| m["content"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    log.lock().unwrap().push(body);
                    if asked.contains("hold") {
                        held.acquire().await.unwrap().forget();
                    }
                    axum::Json(json!({
                        "choices": [{"message": {"role": "assistant", "content": format!("Done: {asked}")},
                                     "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1000, "completion_tokens": 100},
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1/chat/completions", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, gate, seen)
    }

    /// An agent on the stand-in model; `socket`: as on the desktop, where
    /// the signed-in chat's memory commands apply.
    fn agent_on(model: &str, socket: bool) -> (Arc<Agent>, std::path::PathBuf) {
        let (agent, dir) = crate::agent::test_agent();
        let mut cfg = agent.cfg.clone();
        cfg.llm_url = model.to_string();
        if socket {
            cfg.listen = "unix:/run/agentd/agentd.sock".into();
        }
        drop(agent);
        (Arc::new(Agent::new(cfg).unwrap()), dir)
    }

    fn new_session(agent: &Agent) -> String {
        agent.create_session(None).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn queued(posted: Result<Posted, PostError>) -> (String, i64) {
        match posted.unwrap() {
            Posted::Queued { id, position } => (id, position),
            started => panic!("not queued: {started:?}"),
        }
    }

    fn events(agent: &Agent, kind: &str) -> Vec<Value> {
        agent
            .store
            .events_after(0, 10_000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.data)
            .collect()
    }

    /// The queued messages that started, in the order they did.
    fn started(agent: &Agent) -> Vec<String> {
        events(agent, "message.unqueued")
            .into_iter()
            .filter(|e| e["reason"] == "started")
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect()
    }

    async fn until(what: &str, done: impl Fn() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting until {what}");
    }

    /// Parks a task of `session` on a command card, as `drive` would; its
    /// request goes to the model again when it resumes.
    fn park_in(agent: &Agent, session: &str, task: &str, request: &str) -> String {
        agent
            .store
            .start_run_in(
                session,
                task,
                request,
                None,
                "zai-org/GLM-5.3",
                &agent.policy_version(),
                None,
            )
            .unwrap();
        let state = json!({
            "task": task, "model": "zai-org/GLM-5.3",
            "messages": [{"role": "system", "content": ""}, {"role": "user", "content": request}],
            "queue": [], "step": 1, "tainted": false, "reminded": false,
            "loops": crate::guard::LoopDetector::default(), "usage": Usage::default(), "seconds": 1.0,
            "request": request, "session": session,
        });
        let id = format!("card-{task}");
        let card = json!({"kind": "shell", "id": id, "task": task, "command": "ls", "reason": "lists files", "timeout": 120});
        agent
            .store
            .park(&id, task, &card, &state.to_string(), &agent.policy_version())
            .unwrap();
        agent.store.set_run_status(task, "waiting").unwrap();
        id
    }

    #[tokio::test]
    async fn queued_messages_start_oldest_first_one_per_conversation() {
        let (model, gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let c: Vec<String> = (0..4).map(|_| new_session(&agent)).collect();
        // Three conversations work: the limit.
        for session in [MAIN, &c[0], &c[1]] {
            let posted = agent.post(session, "hold on".into(), None).unwrap();
            assert!(matches!(posted, Posted::Started(_)), "{session}");
        }
        assert_eq!(agent.working().len(), 3);
        let (x1, one) = queued(agent.post(&c[2], "first in c3".into(), None));
        let (y1, also_one) = queued(agent.post(&c[3], "first in c4".into(), None));
        let (x2, two) = queued(agent.post(&c[2], "second in c3".into(), None));
        assert_eq!((one, also_one, two), (1, 1, 2), "positions are per conversation");
        assert_eq!(agent.session_view(&c[2]).unwrap()["state"], "queued");
        assert_eq!(
            agent.session_view(&c[2]).unwrap()["queued"][1]["text"],
            "second in c3"
        );
        assert_eq!(agent.sessions_status()["queued"], 3);
        // One slot frees: the oldest queued message whose conversation is
        // free goes first; c3's second waits for its first.
        gate.add_permits(1);
        until("the queue empties", || {
            started(&agent).len() == 3 && agent.working().len() == 2
        })
        .await;
        assert_eq!(started(&agent), [x1, y1, x2]);
        // Each started as the usual message, after leaving the queue.
        let texts: Vec<String> = events(&agent, "message")
            .iter()
            .filter(|m| m["role"] == "user" && m["session"] == c[2].as_str())
            .map(|m| m["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(texts, ["first in c3", "second in c3"]);
        gate.add_permits(2);
        until("everything ends", || agent.working().is_empty()).await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_answered_card_resumes_past_the_limit() {
        let (model, gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let c: Vec<String> = (0..3).map(|_| new_session(&agent)).collect();
        let card = park_in(&agent, &c[2], "t-parked", "hold the resumed task");
        for session in [MAIN, &c[0], &c[1]] {
            assert!(matches!(
                agent.post(session, "hold on".into(), None).unwrap(),
                Posted::Started(_)
            ));
        }
        // The limit is on starting tasks, not finishing them.
        assert!(agent.answer_approval(&card, true, None, None).is_ok());
        assert_eq!(agent.working().len(), 4);
        assert!(agent.is_working_in(&c[2]));
        assert_eq!(events(&agent, "task.resumed")[0]["session"], c[2].as_str());
        gate.add_permits(4);
        until("everything ends", || agent.working().is_empty()).await;
        let finished = events(&agent, "task.finished");
        assert!(
            finished
                .iter()
                .any(|f| f["task"] == "t-parked" && f["status"] == "done")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_new_message_queues_behind_a_parked_card() {
        let (model, _gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let card = park_in(&agent, MAIN, "t1", "list my files");
        let (id, position) = queued(agent.post(MAIN, "and then this".into(), None));
        assert_eq!(position, 1);
        // The card still waits: a new message no longer gives up on it.
        assert!(agent.store.parked(&card).unwrap().is_some());
        assert!(events(&agent, "approval.resolved").is_empty());
        let main = agent.session_view(MAIN).unwrap();
        assert_eq!(
            (main["state"].as_str(), main["needs_you"].as_str()),
            (Some("waiting"), Some("approval"))
        );
        // Answered, the task finishes; then the queued message starts.
        assert!(agent.answer_approval(&card, true, None, None).is_ok());
        until("the queued message ran", || {
            events(&agent, "task.finished").len() == 2 && !agent.is_busy()
        })
        .await;
        assert_eq!(started(&agent), [id]);
        let finished: Vec<Value> = events(&agent, "task.finished")
            .iter()
            .map(|f| f["task"].clone())
            .collect();
        assert_eq!(finished[0], "t1");
        let history: Vec<String> = agent
            .store
            .history_in(MAIN, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.text)
            .collect();
        assert_eq!(
            history,
            ["Done: list my files", "and then this", "Done: and then this"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_memory_command_runs_while_waiting_on_a_card_but_queues_while_working() {
        let (model, gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, true);
        let me = Some("me@example.com".to_string());
        let c: Vec<String> = (0..3).map(|_| new_session(&agent)).collect();
        // Three conversations work, and c3 only waits on a card: the
        // memory command runs at once there, past the limit, and the card
        // stays.
        let card = park_in(&agent, &c[2], "t-parked", "list my files");
        for session in [MAIN, &c[0], &c[1]] {
            agent.post(session, "hold on".into(), me.clone()).unwrap();
        }
        let posted = agent
            .post(&c[2], "remember fact: The office is in Lisbon".into(), me.clone())
            .unwrap();
        assert!(matches!(posted, Posted::Started(_)));
        until("the memory is saved", || {
            events(&agent, "memory.saved").len() == 1
        })
        .await;
        assert!(agent.store.parked(&card).unwrap().is_some());
        assert_eq!(events(&agent, "memory.saved")[0]["session"], c[2].as_str());
        // In a conversation that's working, it waits its turn.
        let (id, _) = queued(agent.post(MAIN, "remember fact: Lunch is at noon".into(), me.clone()));
        gate.add_permits(3);
        until("the queued command ran", || {
            events(&agent, "memory.saved").len() == 2
        })
        .await;
        assert_eq!(started(&agent), [id]);
        until("everything ends", || agent.working().is_empty()).await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn stop_drops_the_conversations_queue() {
        let (model, _gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let Posted::Started(task) = agent.post(MAIN, "hold on".into(), None).unwrap() else {
            panic!("not started")
        };
        queued(agent.post(MAIN, "a".into(), None));
        queued(agent.post(MAIN, "b".into(), None));
        let other = new_session(&agent);
        assert_eq!(
            agent.stop(MAIN),
            Stopped {
                stopping: true,
                stopped: true,
                dropped: 2
            }
        );
        let dropped = events(&agent, "message.unqueued");
        assert_eq!(dropped.len(), 2);
        assert!(
            dropped
                .iter()
                .all(|e| e["reason"] == "stopped" && e["session"] == "main")
        );
        assert_eq!(events(&agent, "task.stopping")[0]["session"], "main");
        until("the task ends", || !agent.is_busy()).await;
        let finished = &events(&agent, "task.finished")[0];
        assert_eq!(
            (finished["task"].as_str(), finished["status"].as_str()),
            (Some(task.as_str()), Some("cancelled"))
        );
        assert!(started(&agent).is_empty(), "nothing queued started after Stop");
        assert!(agent.store.queued_in(MAIN).unwrap().is_empty());
        // Stop on a conversation waiting on a card gives up on it, and its queue.
        let card = park_in(&agent, &other, "t-parked", "list my files");
        queued(agent.post(&other, "x".into(), None));
        assert_eq!(
            agent.stop(&other),
            Stopped {
                stopping: false,
                stopped: true,
                dropped: 1
            }
        );
        assert!(agent.store.parked(&card).unwrap().is_none());
        let resolved = events(&agent, "approval.resolved");
        assert_eq!(
            (resolved[0]["expired"].as_bool(), resolved[0]["session"].as_str()),
            (Some(true), Some(other.as_str()))
        );
        // Nothing to stop.
        assert_eq!(
            agent.stop(&other),
            Stopped {
                stopping: false,
                stopped: false,
                dropped: 0
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_queued_message_can_be_removed() {
        let (model, gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        agent.post(MAIN, "hold on".into(), None).unwrap();
        let (gone, _) = queued(agent.post(MAIN, "never mind".into(), None));
        let (kept, _) = queued(agent.post(MAIN, "this one".into(), None));
        assert!(agent.unqueue(MAIN, &gone).unwrap());
        assert!(!agent.unqueue(MAIN, &gone).unwrap(), "gone already");
        assert!(
            !agent.unqueue("elsewhere", &kept).unwrap(),
            "another conversation's"
        );
        let removed = &events(&agent, "message.unqueued")[0];
        assert_eq!(
            (removed["id"].as_str(), removed["reason"].as_str()),
            (Some(gone.as_str()), Some("removed"))
        );
        let left: Vec<String> = agent
            .store
            .queued_in(MAIN)
            .unwrap()
            .into_iter()
            .map(|q| q.id)
            .collect();
        assert_eq!(left, std::slice::from_ref(&kept));
        gate.add_permits(1);
        until("the kept one ran", || {
            started(&agent) == [kept.clone()] && !agent.is_busy()
        })
        .await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn events_name_their_conversation_and_global_ones_none() {
        let (model, _gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let c1 = new_session(&agent);
        let Posted::Started(task) = agent.post(&c1, "Plan the trip to Lisbon".into(), None).unwrap() else {
            panic!("not started")
        };
        until("the task ends", || !agent.is_working_in(&c1)).await;
        for kind in ["message", "task.started", "task.finished"] {
            let tagged = events(&agent, kind);
            assert!(!tagged.is_empty(), "{kind}");
            assert!(tagged.iter().all(|e| e["session"] == c1.as_str()), "{kind}");
        }
        // Its title came from the first message.
        assert_eq!(
            agent.session_view(&c1).unwrap()["title"],
            "Plan the trip to Lisbon"
        );
        let updated = events(&agent, "session.updated");
        assert!(
            updated.iter().all(|e| e.get("session").is_none()),
            "session.* are global"
        );
        // Global kinds carry none, even naming a task; notes outside a task do.
        agent.emit(
            "access.allowed",
            json!({"hosts": ["a.example.com"], "task": task}),
        );
        agent.say_in(&c1, "A note.");
        assert!(events(&agent, "access.allowed")[0].get("session").is_none());
        assert_eq!(events(&agent, "settings").len(), 0);
        agent.update_settings(None, Some(true)).unwrap();
        assert!(events(&agent, "settings")[0].get("session").is_none());
        // The column follows: a conversation's page has its events and the
        // global ones, not another conversation's.
        agent.say_in(MAIN, "Main's note.");
        let page = agent.store.events_page_in(None, 500, &[], Some(&c1)).unwrap();
        assert!(page.events.iter().any(|e| e.data["text"] == "A note."));
        assert!(page.events.iter().any(|e| e.kind == "settings"));
        assert!(!page.events.iter().any(|e| e.data["text"] == "Main's note."));
        assert!(
            page.events
                .iter()
                .all(|e| e.session.is_none() || e.session.as_deref() == Some(&c1))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn each_conversation_has_its_own_history() {
        let (model, _gate, seen) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let c1 = new_session(&agent);
        for (session, text) in [
            (c1.as_str(), "alpha one"),
            (MAIN, "beta one"),
            (c1.as_str(), "alpha two"),
        ] {
            agent.post(session, text.into(), None).unwrap();
            until("the task ends", || agent.working().is_empty()).await;
        }
        let asked = seen.lock().unwrap().last().unwrap().clone();
        let sent: Vec<&str> = asked["messages"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1) // the system prompt
            .filter_map(|m| m["content"].as_str())
            .collect();
        // Its own earlier exchange, and nothing of main's.
        assert_eq!(sent, ["alpha one", "Done: alpha one", "alpha two"]);
        let texts = |session: &str| -> Vec<String> {
            agent
                .store
                .history_in(session, 20)
                .unwrap()
                .into_iter()
                .map(|h| h.text)
                .collect()
        };
        assert_eq!(texts(&c1).len(), 4);
        assert_eq!(texts(MAIN), ["beta one", "Done: beta one"]);
        let last = &agent.session_view(&c1).unwrap()["last"];
        assert_eq!(
            (last["role"].as_str(), last["text"].as_str()),
            (Some("assistant"), Some("Done: alpha two"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn the_budget_pauses_the_queue_and_raising_it_lets_it_go() {
        let (model, gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        // A card waits in c1 before the budget runs out.
        let c1 = new_session(&agent);
        let card = park_in(&agent, &c1, "t-parked", "list my files");
        agent.set_budget(Some(1.0)).unwrap().unwrap();
        agent
            .store
            .transact(|tx| {
                tx.execute(
                    "INSERT INTO runs (id, status, request, model, cost, session) VALUES ('old', 'done', 'x', 'm', 1.5, 'main')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(agent.budget_paused());
        // New tasks wait, in a conversation that shows it.
        let (id, _) = queued(agent.post(MAIN, "hello".into(), None));
        assert_eq!(agent.session_view(MAIN).unwrap()["state"], "paused");
        assert_eq!(agent.budget().json()["paused"], true);
        // A card answered still resumes.
        assert!(agent.answer_approval(&card, true, None, None).is_ok());
        until("the resumed task ends", || !agent.is_working_in(&c1)).await;
        assert!(started(&agent).is_empty());
        // Raised: the queue goes again.
        agent.set_budget(Some(100.0)).unwrap().unwrap();
        until("the queued message ran", || {
            started(&agent) == [id.clone()] && !agent.is_busy()
        })
        .await;
        // Removed after running out again: it goes too.
        agent.set_budget(Some(1.0)).unwrap().unwrap();
        let (second, _) = queued(agent.post(MAIN, "hold on".into(), None));
        agent.set_budget(None).unwrap().unwrap();
        assert_eq!(started(&agent).last(), Some(&second));
        gate.add_permits(1);
        until("everything ends", || agent.working().is_empty()).await;
        // A bad budget is refused.
        assert!(agent.set_budget(Some(0.0)).unwrap().is_err());
        assert!(agent.set_budget(Some(100_001.0)).unwrap().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn conversations_are_created_renamed_archived_and_deleted() {
        let (model, gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let made = agent.create_session(Some("  Trip   to Lisbon ")).unwrap();
        let id = made["id"].as_str().unwrap().to_string();
        assert_eq!(
            (made["title"].as_str(), made["state"].as_str()),
            (Some("Trip to Lisbon"), Some("idle"))
        );
        assert!(events(&agent, "session.created")[0]["id"] == id.as_str());
        let renamed = agent.update_session(&id, Some("Lisbon"), None).unwrap();
        assert_eq!(renamed["title"], "Lisbon");
        // Working: it can't be archived or deleted.
        agent.post(&id, "hold on".into(), None).unwrap();
        assert!(matches!(
            agent.update_session(&id, None, Some(true)),
            Err(SessionError::Conflict(_))
        ));
        assert!(matches!(
            agent.delete_session(&id),
            Err(SessionError::Conflict(_))
        ));
        gate.add_permits(1);
        until("the task ends", || !agent.is_working_in(&id)).await;
        // Archived: listed apart, and a message for it is refused.
        agent.update_session(&id, None, Some(true)).unwrap();
        let listed = |archived| -> Vec<String> {
            agent
                .sessions_view(archived)
                .unwrap()
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(listed(true), std::slice::from_ref(&id));
        assert_eq!(listed(false), [MAIN.to_string()]);
        assert!(matches!(
            agent.post(&id, "hi".into(), None),
            Err(PostError::NotFound)
        ));
        agent.update_session(&id, None, Some(false)).unwrap();
        // Deleted: its history, queue and open Action needed card go, its
        // runs stay; `main` never.
        let run: String = agent
            .store
            .read(|conn| Ok(conn.query_row("SELECT id FROM runs WHERE session = ?1", [&id], |r| r.get(0))?))
            .unwrap();
        agent.store.set_run_status(&run, "needs_user").unwrap();
        agent.store.add_action("act1", &run).unwrap();
        assert_eq!(agent.store.open_actions().unwrap(), [(run, id.clone())]);
        agent.delete_session(&id).unwrap();
        assert!(agent.store.open_actions().unwrap().is_empty());
        assert!(agent.session_view(&id).is_none());
        assert!(agent.store.history_in(&id, 10).unwrap().is_empty());
        assert!(matches!(agent.delete_session(&id), Err(SessionError::NotFound)));
        assert!(matches!(
            agent.delete_session(MAIN),
            Err(SessionError::Conflict(_))
        ));
        assert_eq!(events(&agent, "session.deleted")[0]["id"], id.as_str());
        let kept: i64 = agent
            .store
            .read(|conn| {
                Ok(
                    conn.query_row("SELECT COUNT(*) FROM runs WHERE session = ?1", [&id], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(kept, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Ages a card, as if it had waited `days` days.
    fn age(agent: &Agent, card: &str, days: i64) {
        agent
            .store
            .transact(|tx| {
                tx.execute(
                    "UPDATE pending SET created = created - ?2 WHERE id = ?1",
                    rusqlite::params![card, days * 86_400],
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn a_card_nobody_answered_for_a_day_doesnt_hold_the_queue() {
        let (model, _gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let card = park_in(&agent, MAIN, "t-old", "list my files");
        age(&agent, &card, 2);
        // A new message gives up on it and starts at once.
        let posted = agent.post(MAIN, "something new".into(), None).unwrap();
        assert!(matches!(posted, Posted::Started(_)), "{posted:?}");
        assert!(agent.store.parked(&card).unwrap().is_none());
        assert_eq!(events(&agent, "approval.resolved")[0]["expired"], true);
        until("the new task ends", || !agent.is_busy()).await;
        let finished = &events(&agent, "task.finished")[0];
        assert_eq!(
            (finished["task"].as_str(), finished["status"].as_str()),
            (Some("t-old"), Some("cancelled"))
        );
        let history: Vec<String> = agent
            .store
            .history_in(MAIN, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.text)
            .collect();
        assert!(
            history[0].starts_with("I stopped waiting for your approval"),
            "{history:?}"
        );
        assert_eq!(history[1..], ["something new", "Done: something new"]);
        // Queued behind a card that then grew old: answering it gives up on
        // it, and what's queued starts at once, not at the next tick.
        let c1 = new_session(&agent);
        let card = park_in(&agent, &c1, "t-c1", "list my files");
        let (id, _) = queued(agent.post(&c1, "behind it".into(), None));
        age(&agent, &card, 2);
        assert!(matches!(
            agent.answer_approval(&card, true, None, None),
            Err(StartError::NotFound)
        ));
        assert_eq!(started(&agent), [id]);
        // The scheduler's tick gives up on them in every conversation.
        let c2 = new_session(&agent);
        let card = park_in(&agent, &c2, "t-c2", "list my files");
        assert!(!agent.expire_stale_cards(None), "a fresh card waits");
        age(&agent, &card, 2);
        assert!(agent.expire_stale_cards(None));
        assert!(agent.store.parked(&card).unwrap().is_none());
        until("everything ends", || agent.working().is_empty()).await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A scheduled run of `session` that ended on Action needed (its card
    /// `card`), keeping its window `tab` for the user.
    fn needs_user_keeping(agent: &Agent, session: &str, run: &str, card: &str, tab: &str) {
        agent
            .store
            .start_run_in(
                session,
                run,
                "Check the build",
                Some("schedule"),
                "zai-org/GLM-5.3",
                "v",
                Some(1),
            )
            .unwrap();
        agent.store.set_run_status(run, "needs_user").unwrap();
        agent.store.add_action(card, run).unwrap();
        agent.store.set_action_tab(run, tab).unwrap();
    }

    #[tokio::test]
    async fn im_done_carries_on_in_the_window_a_scheduled_run_kept() {
        let (model, gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let c1 = new_session(&agent);
        agent.store.set_session_tab(&c1, Some("W1")).unwrap();
        needs_user_keeping(&agent, &c1, "sched1", "act1", "W2");
        // Meanwhile the conversation keeps its own window, and main leaves
        // the kept one alone.
        let tab = |agent: &Agent| agent.store.session(&c1).unwrap().unwrap().tab_target;
        assert_eq!(tab(&agent).as_deref(), Some("W1"));
        assert!(agent.foreign_tabs().contains(&"W2".to_string()));
        assert_eq!(agent.session_view(&c1).unwrap()["needs_you"], "action");
        // "I'm done" while it works: the resume queues, its window with it.
        agent.post(&c1, "hold on".into(), None).unwrap();
        let (id, _) = queued(
            agent
                .resolve_action("act1", true, None)
                .map_err(|_| PostError::NotFound),
        );
        assert_eq!(tab(&agent).as_deref(), Some("W1"), "W1 is in use");
        gate.add_permits(1);
        until("the resume ran", || {
            started(&agent) == [id.clone()] && !agent.is_working_in(&c1)
        })
        .await;
        // When it started, W2 became the conversation's window (W1 closes).
        assert_eq!(tab(&agent).as_deref(), Some("W2"));
        assert!(agent.store.action_tabs().unwrap().is_empty(), "the card is gone");
        assert!(
            agent.foreign_tabs().contains(&"W2".to_string()),
            "now c1's own window"
        );
        // In `main`, the kept window becomes main's tab.
        needs_user_keeping(&agent, MAIN, "sched2", "act2", "W3");
        assert!(matches!(
            agent.resolve_action("act2", false, None),
            Ok(Posted::Started(_))
        ));
        assert_eq!(agent.store.setting("tab_target").unwrap().as_deref(), Some("W3"));
        until("everything ends", || agent.working().is_empty()).await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn im_done_twice_resumes_once() {
        let (model, _gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        agent
            .store
            .start_run("t1", "Sign in to Slack", None, "zai-org/GLM-5.3", "v", None)
            .unwrap();
        agent.store.set_run_status("t1", "needs_user").unwrap();
        agent.store.add_action("act1", "t1").unwrap();
        assert!(matches!(
            agent.resolve_action("act1", true, None),
            Ok(Posted::Started(_))
        ));
        assert!(matches!(
            agent.resolve_action("act1", true, None),
            Err(StartError::NotFound)
        ));
        until("the resume ends", || !agent.is_busy()).await;
        assert_eq!(events(&agent, "task.started").len(), 1);
        assert_eq!(events(&agent, "action.resolved").len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn im_done_in_an_archived_conversation_brings_it_back() {
        let (model, gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let c: Vec<String> = (0..3).map(|_| new_session(&agent)).collect();
        agent
            .store
            .start_run_in(
                &c[2],
                "t1",
                "Sign in to Slack",
                None,
                "zai-org/GLM-5.3",
                "v",
                None,
            )
            .unwrap();
        agent.store.set_run_status("t1", "needs_user").unwrap();
        agent.store.add_action("act1", "t1").unwrap();
        agent.update_session(&c[2], None, Some(true)).unwrap();
        // Every slot taken: the resume queues, in a conversation that's back.
        for session in [MAIN, &c[0], &c[1]] {
            agent.post(session, "hold on".into(), None).unwrap();
        }
        let (id, _) = queued(
            agent
                .resolve_action("act1", true, None)
                .map_err(|_| PostError::NotFound),
        );
        let view = agent.session_view(&c[2]).unwrap();
        assert_eq!(
            (view["archived"].as_bool(), view["state"].as_str()),
            (Some(false), Some("queued"))
        );
        assert!(
            events(&agent, "session.updated")
                .iter()
                .any(|e| e["id"] == c[2].as_str() && e["archived"] == false)
        );
        gate.add_permits(1);
        until("the resume ran", || started(&agent) == [id.clone()]).await;
        gate.add_permits(2);
        until("everything ends", || agent.working().is_empty()).await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_key_saved_later_never_shows_in_a_title() {
        let (model, _gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        let c1 = new_session(&agent);
        agent
            .post(&c1, "Deploy with tok-1234567890abcdef please".into(), None)
            .unwrap();
        until("the task ends", || !agent.is_working_in(&c1)).await;
        assert_eq!(
            agent.session_view(&c1).unwrap()["title"],
            "Deploy with tok-1234567890abcdef please"
        );
        agent.secrets.put("DEPLOY_TOKEN", "tok-1234567890abcdef").unwrap();
        let shown = "Deploy with [secret DEPLOY_TOKEN] please";
        assert_eq!(agent.session_view(&c1).unwrap()["title"], shown);
        assert!(
            agent
                .sessions_view(false)
                .unwrap()
                .iter()
                .any(|s| s["title"] == shown)
        );
        let usage = agent.usage(30).unwrap();
        let line = usage["by_session"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == c1.as_str())
            .unwrap();
        assert_eq!(line["title"], shown);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_task_that_panics_frees_its_conversation() {
        let (model, _gate, _) = gated_model().await;
        let (agent, dir) = agent_on(&model, false);
        // A checkpoint with no messages at all: the resumed task panics.
        agent
            .store
            .start_run(
                "t1",
                "list my files",
                None,
                "zai-org/GLM-5.3",
                &agent.policy_version(),
                None,
            )
            .unwrap();
        let state = json!({
            "task": "t1", "model": "zai-org/GLM-5.3", "messages": [], "queue": [], "step": 1,
            "tainted": false, "reminded": false, "loops": crate::guard::LoopDetector::default(),
            "usage": Usage::default(), "seconds": 1.0,
        });
        let card = json!({"kind": "shell", "id": "a1", "task": "t1", "command": "ls", "timeout": 120});
        agent
            .store
            .park("a1", "t1", &card, &state.to_string(), &agent.policy_version())
            .unwrap();
        let (id, _) = queued(agent.post(MAIN, "next".into(), None));
        assert!(agent.answer_approval("a1", true, None, None).is_ok());
        until("the queued message ran", || {
            started(&agent) == [id.clone()] && !agent.is_busy()
        })
        .await;
        let finished = &events(&agent, "task.finished")[0];
        assert_eq!(
            (finished["task"].as_str(), finished["status"].as_str()),
            (Some("t1"), Some("error"))
        );
        let status: String = agent
            .store
            .read(|conn| Ok(conn.query_row("SELECT status FROM runs WHERE id = 't1'", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(status, "error");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn titles_are_the_first_line_cut_at_a_word() {
        assert_eq!(title_from("  Plan the trip\n to Lisbon"), "Plan the trip");
        assert_eq!(title_from("\n\n  two   spaces  "), "two spaces");
        assert_eq!(title_from(""), "");
        let long = "Find me a flat in Lisbon near the river with two bedrooms and a balcony please";
        let title = title_from(long);
        assert_eq!(
            title,
            "Find me a flat in Lisbon near the river with two bedrooms…"
        );
        assert!(title.chars().count() <= TITLE_CHARS);
        // One long word: cut where it must be.
        assert_eq!(title_from(&"a".repeat(80)).chars().count(), TITLE_CHARS);
    }
}
