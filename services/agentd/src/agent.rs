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
use crate::llm::{Together, Usage, truncate};
use crate::policy::Policy;
use crate::shell::{Shell, split_sudo};
use crate::store::{Event, Store};
use crate::tools;

const HISTORY_LIMIT: i64 = 20;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    pub model: String,
    pub allow_actions: bool,
}

pub enum StartError {
    Busy,
    NotFound,
}

pub struct Agent {
    pub cfg: Config,
    pub store: Store,
    pub bus: broadcast::Sender<Event>,
    llm: Together,
    cdp: Cdp,
    policy: Arc<Policy>,
    shell: Shell,
    busy: AtomicBool,
    cancel: watch::Sender<bool>,
    approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    actions: Mutex<HashSet<String>>,
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// Drops `null` fields: the API rejects some of them when sent back.
fn without_nulls(message: &Value) -> Value {
    match message.as_object() {
        Some(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        None => message.clone(),
    }
}

impl Agent {
    pub fn new(cfg: Config) -> Result<Self> {
        let store = Store::open(&cfg.state_dir.join("agentd.db")).context("opening the state database")?;
        let policy = Arc::new(Policy::load(&cfg.policy_path)?);
        let (bus, _) = broadcast::channel(1024);
        Ok(Self {
            llm: Together::new(cfg.together_api_key.clone())?,
            cdp: Cdp::new(&cfg.cdp_url)?,
            shell: Shell {
                run_as: cfg.run_as.clone(),
                workdir: cfg.workdir.clone(),
            },
            policy,
            store,
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
            allow_actions: get("allow_actions").as_deref() == Some("true"),
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

    pub fn answer_approval(&self, id: &str, approved: bool) -> bool {
        match self.approvals.lock().unwrap().remove(id) {
            Some(reply) => reply.send(approved).is_ok(),
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

        let outcome = 'task: {
            for step in 1..=self.cfg.max_steps {
                if *cancel.borrow() {
                    break 'task ("Stopped.".to_string(), "cancelled", step - 1);
                }
                let request = json!({
                    "model": settings.model, "messages": messages, "tools": schemas,
                    "tool_choice": "auto", "max_tokens": 8192,
                });
                let reply = self.llm.chat(&request).await?;
                usage.add(reply.get("usage"));
                let message = reply["choices"][0]["message"].clone();
                // Send the whole message back, including reasoning_content:
                // GLM and Kimi expect it on every turn.
                messages.push(without_nulls(&message));
                let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
                if calls.is_empty() {
                    let answer = message["content"]
                        .as_str()
                        .unwrap_or("(empty answer)")
                        .trim()
                        .to_string();
                    break 'task (answer, "done", step);
                }
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
                            .call_tool(&mut browser, task_id, &name, &args, &mut cancel)
                            .await
                        {
                            Ok(result) => result,
                            Err(err) => format!("error: {err:#}"),
                        }
                    } else {
                        "error: arguments must be a JSON object".to_string()
                    };
                    messages.push(json!({"role": "tool", "tool_call_id": call["id"], "content": result}));
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
                browser
                    .click(number("ref"), text("role"), text("name"), text("text"))
                    .await
            }
            "type_text" => {
                let submit = args["submit"].as_bool().unwrap_or(false);
                browser
                    .type_text(
                        number("ref"),
                        text("role"),
                        text("name"),
                        required("text")?,
                        submit,
                    )
                    .await
            }
            "press_key" => browser.press_key(required("key")?).await,
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
                self.run_shell(task_id, required("command")?, timeout, cancel)
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
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<String> {
        let (as_root, inner) = split_sudo(command);
        let reason = if as_root {
            Some("runs as root")
        } else {
            self.policy.danger(inner)
        };
        self.emit(
            "shell",
            json!({"task": task_id, "command": command, "as_root": as_root, "needs_approval": reason.is_some()}),
        );
        if let Some(reason) = reason
            && !self.approve(task_id, command, reason, cancel).await
        {
            return Ok(format!(
                "denied: the user did not approve this command (it {reason})"
            ));
        }
        self.shell.run(inner, as_root, timeout).await
    }

    async fn approve(
        &self,
        task_id: &str,
        command: &str,
        reason: &str,
        cancel: &mut watch::Receiver<bool>,
    ) -> bool {
        let id = short_id();
        let (reply, answer) = oneshot::channel();
        self.approvals.lock().unwrap().insert(id.clone(), reply);
        self.emit(
            "approval.requested",
            json!({"task": task_id, "id": id, "command": command, "reason": reason}),
        );
        let approved = tokio::select! {
            result = tokio::time::timeout(APPROVAL_TIMEOUT, answer) => matches!(result, Ok(Ok(true))),
            _ = cancel.wait_for(|stopped| *stopped) => false,
        };
        self.approvals.lock().unwrap().remove(&id);
        self.emit(
            "approval.resolved",
            json!({"task": task_id, "id": id, "approved": approved}),
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
    }
}
