//! The agent: runs one task at a time (there is one browser), emits events for
//! the apps, and asks the user for approvals and "action needed".
//!
//! A task that needs an approval parks: its state is saved, the desktop is free
//! again, and the user's answer resumes it from there (after a restart too).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::browser::Browser;
use crate::cdp::Cdp;
use crate::clock::UserClock;
use crate::config::Config;
use crate::guard;
use crate::llm::{self, Together, Usage, truncate};
use crate::policy::Gate;
use crate::policy::Policy;
use crate::push::{self, Push, Relay};
use crate::shell::{Shell, split_sudo};
use crate::store::{Event, Pending, Store};
use crate::tools;

const HISTORY_LIMIT: i64 = 20;
/// An approval answered later than this doesn't resume its task: the context
/// is stale.
const APPROVAL_TTL_SECS: i64 = 24 * 3600;
const PAGE_CHANGED: &str = "the page changed after the user approved, so nothing was done; look at the page \
     again and ask again if it's still needed";
const INTERRUPTED: &str =
    "I was interrupted: the agent on your desktop restarted. Ask me to continue if you still need this.";

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    pub model: String,
    pub allow_actions: bool,
}

pub enum StartError {
    Busy,
    NotFound,
}

/// A task's state between model turns: all it needs to carry on after parking
/// on an approval, across restarts too.
#[derive(Serialize, Deserialize)]
struct RunState {
    task: String,
    model: String,
    allow_actions: bool,
    messages: Vec<Value>,
    /// Tool calls from the model's last reply not handled yet. When parked, the
    /// first one is waiting for the approval.
    queue: Vec<Value>,
    step: u32,
    /// Outside content (a web page, command output) is in the context: from
    /// then on, commands that can send data out ask first.
    tainted: bool,
    /// Reminded once about needs_user on a sign-in page.
    reminded: bool,
    loops: guard::LoopDetector,
    usage: Usage,
    /// Time spent working, not waiting for the user.
    seconds: f64,
}

/// The user's answer to the approval a resumed task parked on.
struct Answer {
    approved: bool,
    /// The card they answered, to check the action is still the same one.
    card: Value,
}

/// What an approval decided.
enum Decision {
    Approved,
    Denied,
    /// Approved, but the action as it would run now isn't the one on the card.
    Changed,
}

/// Raised when an action needs the user's approval: the task parks until
/// they answer.
#[derive(Debug)]
struct Park {
    id: String,
    card: Value,
}

impl std::fmt::Display for Park {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "waiting for approval {}", self.id)
    }
}

impl std::error::Error for Park {}

enum Outcome {
    Finished(String, &'static str),
    Parked(Park),
}

pub struct Agent {
    pub cfg: Config,
    pub store: Arc<Store>,
    pub bus: broadcast::Sender<Event>,
    llm: Together,
    cdp: Cdp,
    policy: Arc<Policy>,
    shell: Shell,
    /// A task is running (not counting one parked on an approval).
    busy: AtomicBool,
    cancel: watch::Sender<bool>,
    /// Push notifications through the relay; None when it isn't configured.
    pub push: Option<Arc<Relay>>,
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Whether an action, looked at again when its task resumes, is the one the
/// user approved: same kind, element, text, key, site or command.
fn same_request(approved: &Value, now: &Value) -> bool {
    [
        "kind", "action", "role", "target", "text", "key", "submit", "site", "command",
    ]
    .iter()
    .all(|key| approved[key] == now[key])
}

impl Agent {
    pub fn new(cfg: Config) -> Result<Self> {
        let store = Store::open(&cfg.state_dir.join("agentd.db")).context("opening the state database")?;
        let policy = Arc::new(Policy::load(&cfg.policy_path)?);
        let (bus, _) = broadcast::channel(1024);
        let push = Relay::from_config(&cfg)?.map(Arc::new);
        if push.is_some() {
            tracing::info!("push notifications on (relay)");
        }
        let agent = Self {
            push,
            llm: Together::new(&cfg)?,
            cdp: Cdp::new(&cfg.cdp_url)?,
            shell: Shell {
                run_as: cfg.run_as.clone(),
                workdir: cfg.workdir.clone(),
                memory_max: cfg.shell_memory_max.clone(),
                tasks_max: cfg.shell_tasks_max.clone(),
                cpu_quota: cfg.shell_cpu_quota.clone(),
                proxy: cfg.shell_proxy.clone(),
            },
            policy,
            store: Arc::new(store),
            bus,
            busy: AtomicBool::new(false),
            cancel: watch::channel(false).0,
            cfg,
        };
        agent.recover();
        Ok(agent)
    }

    /// After a restart: tasks that were running were cut off, so say so. Parked
    /// ones keep waiting for their approval.
    fn recover(&self) {
        for task in self.store.runs_with_status("running").unwrap_or_default() {
            tracing::warn!(task = %task, "task interrupted by a restart");
            let _ = self.store.add_history("assistant", INTERRUPTED);
            self.emit(
                "message",
                json!({"role": "assistant", "text": INTERRUPTED, "task": task}),
            );
            self.emit("task.finished", json!({"task": task, "status": "interrupted"}));
            let _ = self.store.set_run_status(&task, "interrupted");
        }
    }

    pub fn emit(&self, kind: &str, mut data: Value) {
        // Every event names its agent: "main" until there are bots.
        if let Some(object) = data.as_object_mut() {
            object.entry("agent").or_insert_with(|| json!("main"));
        }
        match self.store.append_event(kind, &data) {
            Ok(event) => {
                let _ = self.bus.send(event);
            }
            Err(err) => tracing::error!(%err, kind, "couldn't store an event"),
        }
    }

    pub fn settings(&self) -> Settings {
        let get = |key: &str| self.store.setting(key).ok().flatten();
        Settings {
            model: get("model").unwrap_or_else(|| self.cfg.model.clone()),
            // On by default: writing and sending in the browser ask first.
            allow_actions: get("allow_actions").as_deref() != Some("false"),
        }
    }

    pub fn update_settings(&self, model: Option<&str>, allow_actions: Option<bool>) -> Result<Settings> {
        if let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) {
            self.store.set_setting("model", model)?;
        }
        if let Some(allow) = allow_actions {
            self.store
                .set_setting("allow_actions", if allow { "true" } else { "false" })?;
        }
        let settings = self.settings();
        self.emit("settings", json!(settings));
        Ok(settings)
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// The approval a parked task waits on, if any.
    pub fn waiting_on(&self) -> Option<String> {
        self.store.any_parked().ok().flatten().map(|p| p.id)
    }

    pub fn start(self: &Arc<Self>, text: String, by: Option<String>) -> Result<String, StartError> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(StartError::Busy);
        }
        // A new request replaces a task that's waiting for an approval.
        if let Ok(Some(parked)) = self.store.any_parked() {
            self.abandon(
                parked,
                "I stopped waiting for your approval because you sent a new message.",
            );
        }
        self.cancel.send_replace(false);
        let task_id = short_id();
        let agent = self.clone();
        let id = task_id.clone();
        tokio::spawn(async move { agent.run(id, text, by).await });
        Ok(task_id)
    }

    pub fn stop(&self) -> bool {
        if self.busy.swap(true, Ordering::SeqCst) {
            // Running: the task stops at its next step (or command).
            self.cancel.send_replace(true);
            return true;
        }
        // Waiting for an approval: stopping it means giving up on it.
        let stopped = match self.store.any_parked() {
            Ok(Some(parked)) => {
                self.abandon(parked, "Stopped.");
                true
            }
            _ => false,
        };
        self.busy.store(false, Ordering::SeqCst);
        stopped
    }

    /// Remembers the user's time zone (sent with each message) for later
    /// tasks, resumed ones included.
    pub fn set_user_clock(&self, clock: &UserClock) {
        let _ = self.store.set_setting("user_timezone", &clock.timezone);
        let _ = self
            .store
            .set_setting("user_utc_offset", &clock.utc_offset_secs.to_string());
    }

    fn user_clock(&self) -> UserClock {
        let get = |key: &str| self.store.setting(key).ok().flatten();
        match (
            get("user_timezone"),
            get("user_utc_offset").and_then(|o| o.parse().ok()),
        ) {
            (Some(timezone), Some(offset)) => UserClock::from_app(&timezone, offset).unwrap_or_default(),
            _ => UserClock::default(),
        }
    }

    /// The user's answer to the approval a task parked on: resumes the task.
    pub fn answer_approval(
        self: &Arc<Self>,
        id: &str,
        approved: bool,
        by: Option<String>,
    ) -> Result<(), StartError> {
        let Ok(Some(parked)) = self.store.parked(id) else {
            return Err(StartError::NotFound);
        };
        if unix_now() - parked.created > APPROVAL_TTL_SECS {
            self.abandon(
                parked,
                "I stopped waiting for your approval: it came a day later, and the desktop has likely \
                 changed since. Ask me again if you still need this.",
            );
            return Err(StartError::NotFound);
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(StartError::Busy);
        }
        let state = parked
            .checkpoint
            .as_deref()
            .and_then(|c| serde_json::from_str::<RunState>(c).ok());
        let (Some(state), Ok(true)) = (state, self.store.remove_pending(id)) else {
            self.busy.store(false, Ordering::SeqCst);
            return Err(StartError::NotFound);
        };
        // The event log records who decided (an audit trail).
        self.emit(
            "approval.resolved",
            json!({"task": parked.run, "id": id, "approved": approved, "by": by}),
        );
        let _ = self.store.set_run_status(&state.task, "running");
        self.cancel.send_replace(false);
        self.emit("task.resumed", json!({"task": state.task}));
        tracing::info!(task = %state.task, approved, by = ?by, "task resumed");
        let agent = self.clone();
        let answer = Answer {
            approved,
            card: parked.card,
        };
        tokio::spawn(async move { agent.drive(state, Some(answer)).await });
        Ok(())
    }

    /// Gives up on a parked task: its approval card expires and the task ends.
    fn abandon(&self, parked: Pending, answer: &str) {
        if !self.store.remove_pending(&parked.id).unwrap_or(false) {
            return; // answered meanwhile
        }
        self.emit(
            "approval.resolved",
            json!({"task": parked.run, "id": parked.id, "approved": false, "expired": true}),
        );
        match parked
            .checkpoint
            .as_deref()
            .and_then(|c| serde_json::from_str::<RunState>(c).ok())
        {
            Some(state) => self.finish(&state, answer, "cancelled"),
            None => {
                self.emit(
                    "task.finished",
                    json!({"task": parked.run, "status": "cancelled"}),
                );
                let _ = self.store.set_run_status(&parked.run, "cancelled");
            }
        }
    }

    /// "I'm done" or "Skip" on an action-needed card: resume with a new task.
    pub fn resolve_action(
        self: &Arc<Self>,
        id: &str,
        done: bool,
        by: Option<String>,
    ) -> Result<String, StartError> {
        if !self.store.has_action(id).unwrap_or(false) {
            return Err(StartError::NotFound);
        }
        let text = if done {
            tools::RESUME_TEXT
        } else {
            tools::SKIP_TEXT
        };
        let task = self.start(text.to_string(), by.clone())?;
        let _ = self.store.remove_pending(id);
        self.emit(
            "action.resolved",
            json!({"id": id, "choice": if done { "done" } else { "skip" }, "by": by}),
        );
        Ok(task)
    }

    /// A screenshot of the current tab, for the app (not part of a task).
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let mut browser = Browser::new(self.cdp.clone(), false, self.policy.clone());
        browser
            .attach(self.store.setting("tab_target")?.as_deref())
            .await?;
        browser.screenshot_jpeg().await
    }

    fn save_image(&self, jpeg: &[u8]) -> Result<String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        std::fs::write(self.cfg.state_dir.join("images").join(format!("{id}.jpg")), jpeg)?;
        Ok(id)
    }

    // --- A task ------------------------------------------------------------------

    async fn run(self: Arc<Self>, task_id: String, text: String, by: Option<String>) {
        let settings = self.settings();
        let history = self.store.history(HISTORY_LIMIT).unwrap_or_default();
        if let Err(err) = self.store.add_history("user", &text) {
            tracing::error!(%err, "couldn't store the message");
        }
        if let Err(err) = self
            .store
            .start_run(&task_id, &text, by.as_deref(), &settings.model)
        {
            tracing::error!(%err, "couldn't store the run");
        }
        self.emit(
            "message",
            json!({"role": "user", "text": text, "task": task_id, "by": by}),
        );
        self.emit("task.started", json!({"task": task_id, "model": settings.model}));
        tracing::info!(task = %task_id, model = %settings.model, by = ?by, "task started");

        // The system prompt is written at each (re)start of the loop.
        let mut messages = vec![json!({"role": "system", "content": ""})];
        messages.extend(history.iter().map(|h| json!({"role": h.role, "content": h.text})));
        messages.push(json!({"role": "user", "content": text}));
        let state = RunState {
            task: task_id,
            model: settings.model,
            allow_actions: settings.allow_actions,
            messages,
            queue: Vec::new(),
            step: 0,
            tainted: false,
            reminded: false,
            loops: guard::LoopDetector::default(),
            usage: Usage::default(),
            seconds: 0.0,
        };
        self.drive(state, None).await;
    }

    /// Runs a task until it finishes or parks on an approval, then frees the
    /// desktop for the next request.
    async fn drive(&self, mut state: RunState, answer: Option<Answer>) {
        let started = Instant::now();
        let result = self.run_loop(&mut state, answer).await;
        state.seconds += started.elapsed().as_secs_f64();
        match result {
            Ok(Outcome::Parked(park)) => match self.save_parked(&state, &park) {
                Ok(()) => {
                    // Free the desktop before asking, so an answer can resume it at once.
                    self.busy.store(false, Ordering::SeqCst);
                    self.ask(&state.task, park);
                    return;
                }
                Err(err) => self.finish(&state, &format!("Something went wrong: {err:#}"), "error"),
            },
            Ok(Outcome::Finished(answer, status)) => self.finish(&state, &answer, status),
            Err(err) => self.finish(&state, &format!("Something went wrong: {err:#}"), "error"),
        }
        self.busy.store(false, Ordering::SeqCst);
    }

    fn save_parked(&self, state: &RunState, park: &Park) -> Result<()> {
        self.store
            .park(&park.id, &state.task, &park.card, &serde_json::to_string(state)?)?;
        self.store.set_run_status(&state.task, "waiting")
    }

    /// Shows a parked task's approval card (and pushes it to the phone).
    fn ask(&self, task_id: &str, park: Park) {
        tracing::info!(task = %task_id, approval = %park.id, "waiting for approval");
        self.notify(approval_push(task_id, &park.id, &park.card));
        self.emit("approval.requested", park.card);
        self.emit("task.paused", json!({"task": task_id, "approval": park.id}));
    }

    /// The answer, to the chat and the history, and the task's end.
    fn finish(&self, state: &RunState, answer: &str, status: &str) {
        let task_id = &state.task;
        if let Err(err) = self.store.add_history("assistant", answer) {
            tracing::error!(%err, "couldn't store the answer");
        }
        self.emit(
            "message",
            json!({"role": "assistant", "text": answer, "task": task_id}),
        );
        let cost = state.usage.cost(&state.model);
        let seconds = state.seconds.round();
        self.emit(
            "task.finished",
            json!({
                "task": task_id, "status": status, "steps": state.step, "seconds": seconds,
                "model": state.model, "usage": state.usage, "cost": cost,
            }),
        );
        if let Err(err) = self.store.finish_run(task_id, status, state.step, cost, answer) {
            tracing::error!(%err, "couldn't store the run's end");
        }
        tracing::info!(task = %task_id, status, steps = state.step, seconds, "task finished");
        // needs_user already pushed its Action needed; a stop came from the user.
        if !matches!(status, "needs_user" | "cancelled") {
            self.notify(Push {
                title: if status == "done" {
                    self.cfg.desktop_name.clone()
                } else {
                    "Your desktop hit a problem".into()
                },
                body: push::clip(answer, 180),
                category: None,
                thread: task_id.clone(),
                data: json!({"task": task_id}),
            });
        }
    }

    async fn run_loop(&self, state: &mut RunState, mut answer: Option<Answer>) -> Result<Outcome> {
        let now = self.user_clock().now();
        // The date and time as of now: a parked task may resume much later.
        state.messages[0] =
            json!({"role": "system", "content": tools::system_prompt(self.cfg.shell_tool, &now)});
        let mut browser = Browser::new(self.cdp.clone(), state.allow_actions, self.policy.clone());
        browser
            .attach(self.store.setting("tab_target")?.as_deref())
            .await?;
        let schemas = tools::schemas(self.cfg.shell_tool);
        let mut cancel = self.cancel.subscribe();
        let task_id = state.task.clone();

        let outcome = 'task: loop {
            // The calls of the model's last reply; after a resume, from the parked one on.
            let mut nudge = None;
            while let Some(call) = state.queue.first().cloned() {
                if *cancel.borrow() {
                    break 'task Outcome::Finished("Stopped.".into(), "cancelled");
                }
                // The user's answer belongs to the first call: the one parked on.
                let resumed = answer.take();
                let step = state.step;
                let name = call["function"]["name"].as_str().unwrap_or_default().to_string();
                let raw = call["function"]["arguments"].as_str().unwrap_or("{}").to_string();
                let args: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                // Shell commands the model doesn't need (a sleep to wait for a
                // page, `date` to learn the date): not run, not shown to the user.
                let refusal = match args["command"].as_str() {
                    Some(command) if name == "run_shell" && guard::is_bare_sleep(command) => {
                        Some(guard::SLEEP_REFUSAL.to_string())
                    }
                    Some(command) if name == "run_shell" && guard::is_bare_date(command) => Some(format!(
                        "not run: you don't need the shell for the date. It's now {now} for the user; \
                         work out other dates from that."
                    )),
                    _ => None,
                };
                if let Some(refusal) = refusal {
                    tracing::info!(task = %task_id, step, "answered a shell command without running it");
                    state.queue.remove(0);
                    if let guard::Repeat::Stop(answer) = state.loops.record(&name, &raw, &refusal) {
                        break 'task Outcome::Finished(answer, "stuck");
                    }
                    state
                        .messages
                        .push(json!({"role": "tool", "tool_call_id": call["id"], "content": refusal}));
                    continue;
                }
                // A resumed call was shown when it was first made.
                if resumed.is_none() {
                    tracing::info!(task = %task_id, step, tool = %name, "tool call");
                    self.emit(
                        "step",
                        json!({"task": task_id, "step": step, "tool": name, "args": truncate(&raw, 300)}),
                    );
                }
                if name == "needs_user" {
                    let reason = args["reason"]
                        .as_str()
                        .unwrap_or("Please take over the desktop.")
                        .to_string();
                    self.action_needed(&browser, &task_id, &reason).await;
                    break 'task Outcome::Finished(format!("I need you: {reason}"), "needs_user");
                }
                let result = if args.is_object() {
                    match self
                        .call_tool(
                            &mut browser,
                            &task_id,
                            &name,
                            &args,
                            state.tainted,
                            resumed,
                            &mut cancel,
                        )
                        .await
                    {
                        Ok(result) => result,
                        Err(err) => match err.downcast::<Park>() {
                            Ok(park) => break 'task Outcome::Parked(park),
                            Err(err) => format!("error: {err:#}"),
                        },
                    }
                } else {
                    "error: arguments must be a JSON object".to_string()
                };
                state.queue.remove(0);
                let repeat = state.loops.record(&name, &raw, &result);
                if guard::carries_outside_content(&name) {
                    state.tainted = true;
                }
                let result = guard::label_untrusted(&name, result);
                state
                    .messages
                    .push(json!({"role": "tool", "tool_call_id": call["id"], "content": result}));
                match repeat {
                    guard::Repeat::Fine => {}
                    guard::Repeat::Nudge(text) => nudge = Some(text),
                    guard::Repeat::Stop(answer) => break 'task Outcome::Finished(answer, "stuck"),
                }
            }
            if let Some(text) = nudge {
                tracing::info!(task = %task_id, step = state.step, "repeated failing call: nudging the model");
                state.messages.push(json!({"role": "user", "content": text}));
            }
            if *cancel.borrow() {
                break 'task Outcome::Finished("Stopped.".into(), "cancelled");
            }
            if state.step >= self.cfg.max_steps {
                break 'task self.wrap_up(state, &schemas).await;
            }

            // The model's next turn.
            state.step += 1;
            let step = state.step;
            let cut = guard::enforce_budget(&mut state.messages, guard::TOOL_OUTPUT_BUDGET);
            if cut > 0 {
                tracing::info!(task = %task_id, step, cut, "trimmed old tool output from the context");
            }
            let request = json!({
                "model": state.model, "messages": state.messages, "tools": schemas,
                "tool_choice": "auto", "max_tokens": 8192,
            });
            let reply = self.llm.chat(&request).await?;
            state.usage.add(reply.get("usage"));
            let message = reply["choices"][0]["message"].clone();
            // Back into the conversation, with reasoning_content for the
            // models that want it (GLM, Kimi) and without for the rest.
            state.messages.push(llm::for_history(&message, &state.model));
            let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
            if calls.is_empty() {
                // Models sometimes ask for a sign-in in their answer, which ends the task
                // with no way for the user to reach the tab. Remind them once.
                if !state.reminded && browser.looks_like_sign_in().await {
                    state.reminded = true;
                    tracing::info!(task = %task_id, step, "sign-in page: reminding about needs_user");
                    state
                        .messages
                        .push(json!({"role": "user", "content": tools::SIGN_IN_REMINDER}));
                    continue;
                }
                let answer = message["content"]
                    .as_str()
                    .unwrap_or("(empty answer)")
                    .trim()
                    .to_string();
                break 'task Outcome::Finished(answer, "done");
            }
            state.queue = calls;
        };
        if let Some(target) = browser.current_target() {
            let _ = self.store.set_setting("tab_target", &target);
        }
        Ok(outcome)
    }

    /// Out of steps: one last turn without tools, so the user hears what was
    /// done and what's left rather than just "stopped".
    async fn wrap_up(&self, state: &mut RunState, schemas: &Value) -> Outcome {
        let fallback = format!("I stopped after {} steps without finishing.", self.cfg.max_steps);
        state
            .messages
            .push(json!({"role": "user", "content": tools::WRAP_UP}));
        guard::enforce_budget(&mut state.messages, guard::TOOL_OUTPUT_BUDGET);
        let request = json!({
            "model": state.model, "messages": state.messages, "tools": schemas,
            "tool_choice": "none", "max_tokens": 2048,
        });
        let answer = match self.llm.chat(&request).await {
            Ok(reply) => {
                state.usage.add(reply.get("usage"));
                reply["choices"][0]["message"]["content"]
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map_or(fallback, String::from)
            }
            Err(err) => {
                tracing::warn!(task = %state.task, %err, "couldn't get a wrap-up");
                fallback
            }
        };
        Outcome::Finished(answer, "max_steps")
    }

    /// Runs a tool call. `resumed`: the user's answer, when the task parked on
    /// this call's approval. A call that needs approval returns a Park error.
    #[allow(clippy::too_many_arguments)]
    async fn call_tool(
        &self,
        browser: &mut Browser,
        task_id: &str,
        name: &str,
        args: &Value,
        tainted: bool,
        resumed: Option<Answer>,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<String> {
        let text = |key: &str| args[key].as_str();
        let required = |key: &str| text(key).ok_or_else(|| anyhow!("{key} is required"));
        // Models sometimes send numbers as strings.
        let number = |key: &str| {
            args[key]
                .as_i64()
                .or_else(|| args[key].as_str()?.trim().parse().ok())
        };
        let max_chars = number("max_chars").map_or(12_000, |n| n.clamp(500, 100_000) as usize);
        match name {
            "list_tabs" => browser.list_tabs().await,
            "switch_tab" => {
                browser
                    .switch_tab(number("index").ok_or_else(|| anyhow!("index is required"))? as usize)
                    .await
            }
            "open_url" => {
                let url = required("url")?;
                if let Err(reason) = guard::check_url(url) {
                    return Ok(format!("blocked: {reason}"));
                }
                browser
                    .open_url(url, args["new_tab"].as_bool().unwrap_or(false))
                    .await
            }
            "read_text" => browser.read_text(max_chars).await,
            "snapshot" => browser.snapshot(max_chars).await,
            // When resumed, the element is found again and the gate checks it's
            // the one on the card the user approved (same role, label and site).
            "click" => {
                let element = browser
                    .find(number("ref"), text("role"), text("name"), text("text"))
                    .await?;
                let gate = browser.click_gate(&element);
                let card = json!({
                    "action": "click", "target": element.label, "role": element.role, "site": browser.host().await,
                });
                match self.gate(task_id, gate, card, resumed)? {
                    Err(refusal) => Ok(refusal),
                    Ok(()) => browser.click(&element).await,
                }
            }
            "type_text" => {
                let submit = args["submit"].as_bool().unwrap_or(false);
                let value = required("text")?;
                let element = browser
                    .find(number("ref"), text("role"), text("name"), None)
                    .await?;
                let gate = browser.type_gate(&element, submit);
                let card = json!({
                    "action": "type", "target": element.label, "role": element.role, "text": truncate(value, 4000),
                    "submit": submit, "site": browser.host().await,
                });
                match self.gate(task_id, gate, card, resumed)? {
                    Err(refusal) => Ok(refusal),
                    Ok(()) => browser.type_text(&element, value, submit).await,
                }
            }
            "press_key" => {
                let key = required("key")?;
                let focused = browser.focused().await;
                let gate = browser.key_gate(key, &focused);
                let card = json!({
                    "action": "key", "key": key, "target": focused.label, "role": focused.role,
                    "site": browser.host().await,
                });
                match self.gate(task_id, gate, card, resumed)? {
                    Err(refusal) => Ok(refusal),
                    Ok(()) => browser.press_key(key).await,
                }
            }
            "wait_for" => {
                let timeout = number("timeout").map_or(10, |n| n.max(1) as u64);
                browser.wait_for(text("text"), timeout).await
            }
            "show_screenshot" => {
                let id = self.save_image(&browser.screenshot_jpeg().await?)?;
                self.emit(
                    "image",
                    json!({"task": task_id, "id": id, "url": format!("/api/v1/images/{id}"), "caption": text("caption")}),
                );
                Ok("shown to the user".into())
            }
            "run_shell" if self.cfg.shell_tool => {
                let timeout = number("timeout").map_or(120, |n| n.max(1) as u64);
                self.run_shell(task_id, required("command")?, timeout, tainted, resumed, cancel)
                    .await
            }
            _ => Ok(format!("error: unknown tool {name}")),
        }
    }

    async fn run_shell(
        &self,
        task_id: &str,
        command: &str,
        timeout: u64,
        tainted: bool,
        resumed: Option<Answer>,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<String> {
        let (as_root, inner) = split_sudo(command);
        let reason = if as_root {
            Some("runs as root")
        } else if let Some(reason) = self.policy.danger(inner) {
            Some(reason)
        } else if tainted && self.policy.sends_data(inner) {
            // Injected instructions in a page could try to send data out.
            Some("could send data off the desktop, after reading outside content")
        } else {
            None
        };
        // A resumed command's terminal card is already showing, waiting on the approval.
        if resumed.is_none() {
            self.emit(
                "shell",
                json!({"task": task_id, "command": command, "as_root": as_root, "needs_approval": reason.is_some()}),
            );
        }
        if let Some(reason) = reason {
            let card = json!({"kind": "shell", "command": command, "reason": reason});
            if !matches!(self.approve(task_id, card, resumed)?, Decision::Approved) {
                return Ok(format!(
                    "denied: the user did not approve this command (it {reason})"
                ));
            }
        }
        let started = std::time::Instant::now();
        let output = self.shell.run(inner, as_root, timeout, cancel).await?;
        // The app shows each command as a terminal card with its output.
        self.emit(
            "shell.output",
            json!({
                "task": task_id, "command": command, "exit_code": output.code,
                "timed_out": output.timed_out, "stopped": output.stopped, "output": output.tail(4000),
                "seconds": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            }),
        );
        Ok(output.for_model())
    }

    /// Runs a browser action's gate: `Ok(Ok(()))` to go ahead, `Ok(Err(text
    /// for the model))` if refused or not approved; a Park error to ask.
    fn gate(
        &self,
        task_id: &str,
        gate: Gate,
        mut card: Value,
        resumed: Option<Answer>,
    ) -> Result<Result<(), String>> {
        match gate {
            Gate::Allow => Ok(Ok(())),
            Gate::Block(refusal) => Ok(Err(refusal)),
            Gate::Ask(reason) => {
                card["kind"] = json!("browser");
                card["reason"] = json!(reason);
                Ok(match self.approve(task_id, card, resumed)? {
                    Decision::Approved => Ok(()),
                    Decision::Changed => Err(PAGE_CHANGED.into()),
                    Decision::Denied => Err(format!(
                        "denied: the user did not approve this (it {reason}). Don't try another way; \
                         tell the user what you would have done."
                    )),
                })
            }
        }
    }

    /// Asks the user about an action. A resumed task brings the answer to the
    /// card it parked on; otherwise this returns a Park error: the task parks
    /// and the card goes to the apps.
    fn approve(&self, task_id: &str, mut card: Value, resumed: Option<Answer>) -> Result<Decision> {
        card["task"] = json!(task_id);
        match resumed {
            Some(answer) if !answer.approved => Ok(Decision::Denied),
            // The page may have changed while the user decided: act only on
            // what the card showed.
            Some(answer) if !same_request(&answer.card, &card) => Ok(Decision::Changed),
            Some(_) => Ok(Decision::Approved),
            None => {
                let id = short_id();
                card["id"] = json!(id);
                Err(Park { id, card }.into())
            }
        }
    }

    async fn action_needed(&self, browser: &Browser, task_id: &str, reason: &str) {
        let id = short_id();
        let image = match browser.screenshot_jpeg().await {
            Ok(jpeg) => self.save_image(&jpeg).ok().map(|i| format!("/api/v1/images/{i}")),
            Err(_) => None,
        };
        let page = browser.where_am_i().await.unwrap_or_default();
        if let Err(err) = self.store.add_action(&id, task_id) {
            tracing::error!(%err, "couldn't store the action");
        }
        self.emit(
            "action_needed",
            json!({"task": task_id, "id": id, "reason": reason, "page": page, "image": image}),
        );
        self.notify(Push {
            title: "Action needed".into(),
            body: push::clip(reason, 180),
            category: Some("ACTION"),
            thread: task_id.to_string(),
            data: json!({"task": task_id, "action": id}),
        });
    }

    /// Notifies the user's devices through the relay, in the background.
    fn notify(&self, notification: Push) {
        let Some(relay) = self.push.clone() else { return };
        tokio::spawn(async move {
            if let Err(err) = relay.notify(&notification).await {
                tracing::warn!(%err, "push failed");
            }
        });
    }
}

/// The notification for an approval: what will run or be written, and where.
fn approval_push(task_id: &str, id: &str, details: &Value) -> Push {
    let text = |key: &str| details[key].as_str().unwrap_or_default().to_string();
    let site = text("site");
    let (title, body) = match (details["kind"].as_str(), details["action"].as_str()) {
        (Some("browser"), Some("type")) => (
            format!("Write this on {site}?"),
            format!("In “{}”: {}", text("target"), text("text")),
        ),
        (Some("browser"), Some("key")) => (
            format!("Press {} on {site}?", text("key")),
            format!("In “{}”. It {}.", text("target"), text("reason")),
        ),
        (Some("browser"), _) => (
            format!("Click this on {site}?"),
            format!("“{}”. It {}.", text("target"), text("reason")),
        ),
        _ => (
            "Run this command?".into(),
            format!("{} (it {})", text("command"), text("reason")),
        ),
    };
    Push {
        title,
        body: push::clip(&body, 180),
        category: Some("APPROVAL"),
        thread: task_id.to_string(),
        data: json!({"task": task_id, "approval": id}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resumed_action_must_match_its_card() {
        let approved = json!({
            "kind": "browser", "action": "type", "role": "textbox", "target": "Message #eng",
            "text": "Shipping today", "submit": true, "site": "app.slack.com", "reason": "sends",
            "task": "t1", "id": "a1",
        });
        // The same action looked at again (no id yet, another reason wording).
        let mut now = approved.clone();
        now.as_object_mut().unwrap().remove("id");
        now["reason"] = json!("could send a message");
        assert!(same_request(&approved, &now));
        for (key, value) in [
            ("target", json!("Message #random")),
            ("site", json!("evil.example")),
            ("text", json!("Shipping tomorrow")),
            ("role", json!("button")),
        ] {
            let mut changed = now.clone();
            changed[key] = value;
            assert!(!same_request(&approved, &changed), "{key}");
        }
        let command = json!({"kind": "shell", "command": "sudo apt install jq", "task": "t1"});
        assert!(same_request(&command, &command));
        assert!(!same_request(
            &command,
            &json!({"kind": "shell", "command": "sudo rm -rf /", "task": "t1"})
        ));
    }
}
