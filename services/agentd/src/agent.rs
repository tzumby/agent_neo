//! The agent: runs one task at a time (there is one browser), emits events for
//! the apps, and waits on the user for approvals and "action needed".

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot, watch};

use crate::browser::Browser;
use crate::cdp::Cdp;
use crate::config::Config;
use crate::guard;
use crate::llm::{self, Together, Usage, truncate};
use crate::policy::Gate;
use crate::policy::Policy;
use crate::push::{self, Push, Relay};
use crate::shell::{Shell, split_sudo};
use crate::store::{Event, Store};
use crate::tools;

const HISTORY_LIMIT: i64 = 20;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);
const PAGE_CHANGED: &str = "the page changed after the user approved, so nothing was done; look at the page \
     again and ask again if it's still needed";

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    pub model: String,
    pub allow_actions: bool,
}

pub enum StartError {
    Busy,
    NotFound,
}

/// An approval's answer and who gave it.
type ApprovalReply = oneshot::Sender<(bool, Option<String>)>;

pub struct Agent {
    pub cfg: Config,
    pub store: Arc<Store>,
    pub bus: broadcast::Sender<Event>,
    llm: Together,
    cdp: Cdp,
    policy: Arc<Policy>,
    shell: Shell,
    busy: AtomicBool,
    cancel: watch::Sender<bool>,
    /// Pending approvals: the answer and who gave it (their tailnet login).
    approvals: Mutex<HashMap<String, ApprovalReply>>,
    actions: Mutex<HashSet<String>>,
    /// Push notifications through the relay; None when it isn't configured.
    pub push: Option<Arc<Relay>>,
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
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
        Ok(Self {
            push,
            llm: Together::new(cfg.together_api_key.clone())?,
            cdp: Cdp::new(&cfg.cdp_url)?,
            shell: Shell {
                run_as: cfg.run_as.clone(),
                workdir: cfg.workdir.clone(),
                memory_max: cfg.shell_memory_max.clone(),
                tasks_max: cfg.shell_tasks_max.clone(),
                cpu_quota: cfg.shell_cpu_quota.clone(),
            },
            policy,
            store: Arc::new(store),
            bus,
            busy: AtomicBool::new(false),
            cancel: watch::channel(false).0,
            approvals: Mutex::default(),
            actions: Mutex::default(),
            cfg,
        })
    }

    pub fn emit(&self, kind: &str, data: Value) {
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

    pub fn start(self: &Arc<Self>, text: String, by: Option<String>) -> Result<String, StartError> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(StartError::Busy);
        }
        self.cancel.send_replace(false);
        let task_id = short_id();
        let agent = self.clone();
        let id = task_id.clone();
        tokio::spawn(async move { agent.run(id, text, by).await });
        Ok(task_id)
    }

    pub fn stop(&self) -> bool {
        let busy = self.is_busy();
        if busy {
            self.cancel.send_replace(true);
        }
        busy
    }

    pub fn answer_approval(&self, id: &str, approved: bool, by: Option<String>) -> bool {
        match self.approvals.lock().unwrap().remove(id) {
            Some(reply) => reply.send((approved, by)).is_ok(),
            None => false,
        }
    }

    /// "I'm done" or "Skip" on an action-needed card: resume with a new task.
    pub fn resolve_action(
        self: &Arc<Self>,
        id: &str,
        done: bool,
        by: Option<String>,
    ) -> Result<String, StartError> {
        if !self.actions.lock().unwrap().contains(id) {
            return Err(StartError::NotFound);
        }
        let text = if done {
            tools::RESUME_TEXT
        } else {
            tools::SKIP_TEXT
        };
        let task = self.start(text.to_string(), by.clone())?;
        self.actions.lock().unwrap().remove(id);
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
        let started = Instant::now();
        let settings = self.settings();
        let history = self.store.history(HISTORY_LIMIT).unwrap_or_default();
        if let Err(err) = self.store.add_history("user", &text) {
            tracing::error!(%err, "couldn't store the message");
        }
        self.emit(
            "message",
            json!({"role": "user", "text": text, "task": task_id, "by": by}),
        );
        self.emit("task.started", json!({"task": task_id, "model": settings.model}));
        tracing::info!(task = %task_id, model = %settings.model, by = ?by, "task started");

        let mut usage = Usage::default();
        let (answer, status, steps) = match self
            .run_loop(&task_id, &text, &history, &settings, &mut usage)
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => (format!("Something went wrong: {err:#}"), "error", 0),
        };

        if let Err(err) = self.store.add_history("assistant", &answer) {
            tracing::error!(%err, "couldn't store the answer");
        }
        self.emit(
            "message",
            json!({"role": "assistant", "text": answer, "task": task_id}),
        );
        let seconds = started.elapsed().as_secs_f64().round();
        self.emit(
            "task.finished",
            json!({
                "task": task_id, "status": status, "steps": steps, "seconds": seconds,
                "model": settings.model, "usage": usage, "cost": usage.cost(&settings.model),
            }),
        );
        tracing::info!(task = %task_id, status, steps, seconds, "task finished");
        // needs_user already pushed its Action needed; a stop came from the user.
        if !matches!(status, "needs_user" | "cancelled") {
            self.notify(Push {
                title: if status == "done" {
                    self.cfg.desktop_name.clone()
                } else {
                    "Your desktop hit a problem".into()
                },
                body: push::clip(&answer, 180),
                category: None,
                thread: task_id.clone(),
                data: json!({"task": task_id}),
            });
        }
        self.busy.store(false, Ordering::SeqCst);
    }

    async fn run_loop(
        &self,
        task_id: &str,
        text: &str,
        history: &[crate::store::HistoryEntry],
        settings: &Settings,
        usage: &mut Usage,
    ) -> Result<(String, &'static str, u32)> {
        let mut browser = Browser::new(self.cdp.clone(), settings.allow_actions, self.policy.clone());
        browser
            .attach(self.store.setting("tab_target")?.as_deref())
            .await?;
        let mut messages =
            vec![json!({"role": "system", "content": tools::system_prompt(self.cfg.shell_tool)})];
        messages.extend(history.iter().map(|h| json!({"role": h.role, "content": h.text})));
        messages.push(json!({"role": "user", "content": text}));
        let schemas = tools::schemas(self.cfg.shell_tool);
        let mut cancel = self.cancel.subscribe();
        let mut reminded = false;
        let mut loops = guard::LoopDetector::default();
        // Set once outside content (a web page, command output) is in the
        // context: from then on, commands that can send data out ask first.
        let mut tainted = false;

        let outcome = 'task: {
            for step in 1..=self.cfg.max_steps {
                if *cancel.borrow() {
                    break 'task ("Stopped.".to_string(), "cancelled", step - 1);
                }
                let cut = guard::enforce_budget(&mut messages, guard::TOOL_OUTPUT_BUDGET);
                if cut > 0 {
                    tracing::info!(task = %task_id, step, cut, "trimmed old tool output from the context");
                }
                let request = json!({
                    "model": settings.model, "messages": messages, "tools": schemas,
                    "tool_choice": "auto", "max_tokens": 8192,
                });
                let reply = self.llm.chat(&request).await?;
                usage.add(reply.get("usage"));
                let message = reply["choices"][0]["message"].clone();
                // Back into the conversation, with reasoning_content for the
                // models that want it (GLM, Kimi) and without for the rest.
                messages.push(llm::for_history(&message, &settings.model));
                let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
                if calls.is_empty() {
                    // Models sometimes ask for a sign-in in their answer, which ends the task
                    // with no way for the user to reach the tab. Remind them once.
                    if !reminded && browser.looks_like_sign_in().await {
                        reminded = true;
                        tracing::info!(task = %task_id, step, "sign-in page: reminding about needs_user");
                        messages.push(json!({"role": "user", "content": tools::SIGN_IN_REMINDER}));
                        continue;
                    }
                    let answer = message["content"]
                        .as_str()
                        .unwrap_or("(empty answer)")
                        .trim()
                        .to_string();
                    break 'task (answer, "done", step);
                }
                let mut nudge = None;
                for call in calls {
                    let name = call["function"]["name"].as_str().unwrap_or_default().to_string();
                    let raw = call["function"]["arguments"].as_str().unwrap_or("{}").to_string();
                    tracing::info!(task = %task_id, step, tool = %name, "tool call");
                    self.emit(
                        "step",
                        json!({"task": task_id, "step": step, "tool": name, "args": truncate(&raw, 300)}),
                    );
                    let args: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                    if name == "needs_user" {
                        let reason = args["reason"]
                            .as_str()
                            .unwrap_or("Please take over the desktop.")
                            .to_string();
                        self.action_needed(&browser, task_id, &reason).await;
                        break 'task (format!("I need you: {reason}"), "needs_user", step);
                    }
                    let result = if args.is_object() {
                        match self
                            .call_tool(&mut browser, task_id, &name, &args, tainted, &mut cancel)
                            .await
                        {
                            Ok(result) => result,
                            Err(err) => format!("error: {err:#}"),
                        }
                    } else {
                        "error: arguments must be a JSON object".to_string()
                    };
                    let repeat = loops.record(&name, &raw, &result);
                    if guard::carries_outside_content(&name) {
                        tainted = true;
                    }
                    let result = guard::label_untrusted(&name, result);
                    messages.push(json!({"role": "tool", "tool_call_id": call["id"], "content": result}));
                    match repeat {
                        guard::Repeat::Fine => {}
                        guard::Repeat::Nudge(text) => nudge = Some(text),
                        guard::Repeat::Stop(answer) => break 'task (answer, "stuck", step),
                    }
                }
                if let Some(text) = nudge {
                    tracing::info!(task = %task_id, step, "repeated failing call: nudging the model");
                    messages.push(json!({"role": "user", "content": text}));
                }
            }
            (
                format!("Stopped after {} steps without finishing.", self.cfg.max_steps),
                "max_steps",
                self.cfg.max_steps,
            )
        };
        if let Some(target) = browser.current_target() {
            let _ = self.store.set_setting("tab_target", &target);
        }
        Ok(outcome)
    }

    async fn call_tool(
        &self,
        browser: &mut Browser,
        task_id: &str,
        name: &str,
        args: &Value,
        tainted: bool,
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
                browser
                    .open_url(required("url")?, args["new_tab"].as_bool().unwrap_or(false))
                    .await
            }
            "read_text" => browser.read_text(max_chars).await,
            "snapshot" => browser.snapshot(max_chars).await,
            "click" => {
                let find = (number("ref"), text("role"), text("name"), text("text"));
                let element = browser.find(find.0, find.1, find.2, find.3).await?;
                let gate = browser.click_gate(&element);
                let card = json!({"action": "click", "target": element.label, "site": browser.host().await});
                match self.gate(task_id, gate, card, cancel).await {
                    Err(refusal) => Ok(refusal),
                    // The page may have changed while the user decided: act only
                    // on the element the card showed.
                    Ok(true) => {
                        let now = browser.find(find.0, find.1, find.2, find.3).await?;
                        if now.label != element.label || now.role != element.role {
                            return Ok(PAGE_CHANGED.into());
                        }
                        browser.click(&now).await
                    }
                    Ok(false) => browser.click(&element).await,
                }
            }
            "type_text" => {
                let submit = args["submit"].as_bool().unwrap_or(false);
                let value = required("text")?;
                let find = (number("ref"), text("role"), text("name"));
                let element = browser.find(find.0, find.1, find.2, None).await?;
                let gate = browser.type_gate(&element, submit);
                let card = json!({
                    "action": "type", "target": element.label, "text": truncate(value, 4000),
                    "submit": submit, "site": browser.host().await,
                });
                match self.gate(task_id, gate, card, cancel).await {
                    Err(refusal) => Ok(refusal),
                    Ok(true) => {
                        let now = browser.find(find.0, find.1, find.2, None).await?;
                        if now.label != element.label || now.role != element.role {
                            return Ok(PAGE_CHANGED.into());
                        }
                        browser.type_text(&now, value, submit).await
                    }
                    Ok(false) => browser.type_text(&element, value, submit).await,
                }
            }
            "press_key" => {
                let key = required("key")?;
                let gate = browser.key_gate(key);
                let card = json!({
                    "action": "key", "key": key, "target": browser.focused().await, "site": browser.host().await,
                });
                match self.gate(task_id, gate, card, cancel).await {
                    Err(refusal) => Ok(refusal),
                    Ok(_) => browser.press_key(key).await,
                }
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
                self.run_shell(task_id, required("command")?, timeout, tainted, cancel)
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
        self.emit(
            "shell",
            json!({"task": task_id, "command": command, "as_root": as_root, "needs_approval": reason.is_some()}),
        );
        if let Some(reason) = reason
            && !self
                .approve(
                    task_id,
                    json!({"kind": "shell", "command": command, "reason": reason}),
                    cancel,
                )
                .await
        {
            return Ok(format!(
                "denied: the user did not approve this command (it {reason})"
            ));
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

    /// Runs a browser action's gate: `Ok(asked)` to go ahead (`asked`: after
    /// the user approved), `Err(text for the model)` if refused or denied.
    async fn gate(
        &self,
        task_id: &str,
        gate: Gate,
        mut card: Value,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<bool, String> {
        match gate {
            Gate::Allow => Ok(false),
            Gate::Block(refusal) => Err(refusal),
            Gate::Ask(reason) => {
                card["kind"] = json!("browser");
                card["reason"] = json!(reason);
                if self.approve(task_id, card, cancel).await {
                    Ok(true)
                } else {
                    Err(format!(
                        "denied: the user did not approve this (it {reason}). Don't try another way; \
                         tell the user what you would have done."
                    ))
                }
            }
        }
    }

    /// Asks the user (an approval card in the apps) and waits for the answer.
    /// `details` describes what's asked: a shell command, or a browser action.
    async fn approve(&self, task_id: &str, mut details: Value, cancel: &mut watch::Receiver<bool>) -> bool {
        let id = short_id();
        let (reply, answer) = oneshot::channel();
        self.approvals.lock().unwrap().insert(id.clone(), reply);
        details["task"] = json!(task_id);
        details["id"] = json!(id);
        self.notify(approval_push(task_id, &id, &details));
        self.emit("approval.requested", details);
        let (approved, by) = tokio::select! {
            result = tokio::time::timeout(APPROVAL_TIMEOUT, answer) => match result {
                Ok(Ok(answer)) => answer,
                _ => (false, None),
            },
            _ = cancel.wait_for(|stopped| *stopped) => (false, None),
        };
        self.approvals.lock().unwrap().remove(&id);
        // The event log records who decided (an audit trail).
        self.emit(
            "approval.resolved",
            json!({"task": task_id, "id": id, "approved": approved, "by": by}),
        );
        approved
    }

    async fn action_needed(&self, browser: &Browser, task_id: &str, reason: &str) {
        let id = short_id();
        let image = match browser.screenshot_jpeg().await {
            Ok(jpeg) => self.save_image(&jpeg).ok().map(|i| format!("/api/v1/images/{i}")),
            Err(_) => None,
        };
        let page = browser.where_am_i().await.unwrap_or_default();
        self.actions.lock().unwrap().insert(id.clone());
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
