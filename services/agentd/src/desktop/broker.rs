//! The desktop broker: `agentd desktop-broker`, run as the desktop user by
//! systemd (docs/desktop-apps.md §3). It holds one `computer-use mcp` process
//! on the XFCE session and answers agentd alone, over a socket systemd made
//! (`agentd`, 0600). Each connection's peer is checked too: the socket's
//! group, `agentd`, also has tailnet-proxy in it.
//!
//! What it adds to HCU:
//! - typed operations only, on the windows of the apps the deployment lists
//!   (by AT-SPI app id and executable), and clicks only through the app's
//!   accessibility actions (`allow_global_input: false`);
//! - one request at a time, so input never interleaves;
//! - each task's refs: a task uses only refs it was shown since it last took
//!   over HCU's observation context; another task's use voids them;
//! - an approved click goes ahead only if the element is still what the
//!   approval showed, checked under the same lock as the click;
//! - after input whose outcome is unknown (an uncertain result, a lost
//!   connection, HCU stopping mid-call), no more input until someone has
//!   looked at that window again.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use super::hcu::{Hcu, Spawn, ToolResult};
use super::steps::{self, GuiAction, Step};
use super::{
    Binding, GuiBinding, GuiTarget, Image, OffBus, Query, Request, Response, TextPage, WindowLine, evidence,
    find_element, input_uncertain, parse_apps, refs, refusal,
};

/// How long a request waits for the one before it.
const LOCK_WAIT: Duration = Duration::from_secs(60);
const READ_DEADLINE: Duration = Duration::from_secs(30);
/// A batch's input and the diff after it; its waits come on top.
const ACT_DEADLINE: Duration = Duration::from_secs(60);
/// Uncertain pixel input waits on a look at the whole desktop.
const DESKTOP: &str = "desktop";
const NEED_SCREENSHOT: &str = "take a screenshot first (app_screenshot): pixel actions use the latest one, and the desktop has been read or changed since";
const MAX_REQUEST: u64 = 64 * 1024;

/// /etc/neo-desktop/broker.toml (Ansible's).
#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    /// The only user that may connect.
    pub client_user: String,
    pub computer_use: PathBuf,
    /// A socket to bind when systemd didn't pass one (local runs).
    #[serde(default)]
    pub socket: Option<PathBuf>,
    /// The session's environment, and HCU's settings.
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub schema_sha256: BTreeMap<String, String>,
    pub apps: Vec<AppRule>,
}

/// An app the agent may use: AT-SPI's id for it, its executable, and the
/// name people know it by.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct AppRule {
    pub id: String,
    pub exe: String,
    pub name: String,
}

/// HCU and the desktop, as the broker uses them: the live ones or a test's.
pub trait Backend: Send + 'static {
    /// (Re)starts HCU; its version.
    fn start(&mut self) -> impl Future<Output = Result<String>> + Send;
    fn running(&mut self) -> bool;
    fn stop(&mut self);
    fn call(
        &mut self,
        tool: &str,
        arguments: Value,
        deadline: Duration,
    ) -> impl Future<Output = Result<ToolResult>> + Send;
    /// The processes whose top-level windows carry this title: pid, executable.
    fn owners(&mut self, title: &str) -> impl Future<Output = Result<Vec<(u32, String)>>> + Send;
    /// A process's executable.
    fn exe(&mut self, pid: u32) -> Option<String>;
    /// The top-level window under a screen pixel: its title and process.
    fn window_at(&mut self, x: i64, y: i64) -> impl Future<Output = Result<Option<(String, u32)>>> + Send;
    /// The window with the keyboard focus: its title and process.
    fn active_window(&mut self) -> impl Future<Output = Result<Option<(String, u32)>>> + Send;
    fn screen_size(&mut self) -> impl Future<Output = Result<(u32, u32)>> + Send;
}

/// A window in the latest listing, and whether the agent may use it.
#[derive(Debug, Clone)]
struct Window {
    line: WindowLine,
    rule: Option<AppRule>,
    pid: u32,
    exe: String,
}

impl Window {
    fn allowed(&self) -> bool {
        self.rule.is_some() && !self.line.no_tree
    }
}

/// Input whose outcome nobody has seen yet.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    epoch: String,
    window: String,
    token: u64,
}

struct State<B> {
    backend: B,
    /// The running HCU process's name (refs belong to it); None until started.
    epoch: Option<String>,
    starts: u64,
    /// The task whose refs are current, and the refs it was shown.
    owner: Option<String>,
    issued: BTreeSet<String>,
    windows: BTreeMap<String, Window>,
    pending: Option<Pending>,
    tokens: u64,
    /// HCU's frames so far (screenshots, full observations, pixel batches),
    /// and the one a task's model last saw: pixel input is planned on it.
    frames: u64,
    model_frame: Option<ModelFrame>,
}

#[derive(Debug, Clone)]
struct ModelFrame {
    task: String,
    seq: u64,
    size: (u32, u32),
}

pub struct Broker<B> {
    boot: String,
    rules: Vec<AppRule>,
    state: Mutex<State<B>>,
}

/// What a click leaves to do once its result reached agentd.
pub struct Delivery(Option<u64>);

impl<B: Backend> Broker<B> {
    pub fn new(backend: B, rules: Vec<AppRule>) -> Self {
        Self {
            boot: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            rules,
            state: Mutex::new(State {
                backend,
                epoch: None,
                starts: 0,
                owner: None,
                issued: BTreeSet::new(),
                windows: BTreeMap::new(),
                pending: None,
                tokens: 0,
                frames: 0,
                model_frame: None,
            }),
        }
    }

    /// A click's result reached agentd: its input is seen, unless something
    /// uncertain has happened since.
    pub async fn delivered(&self, delivery: Delivery) {
        let Some(token) = delivery.0 else { return };
        let mut state = self.state.lock().await;
        if state.pending.as_ref().is_some_and(|p| p.token == token) {
            state.pending = None;
        }
    }

    pub async fn handle(&self, request: Request) -> (Response, Delivery) {
        let Ok(mut state) = tokio::time::timeout(LOCK_WAIT, self.state.lock()).await else {
            return (
                Response::refused(
                    refusal::BUSY,
                    "the desktop is busy with another request; try again",
                ),
                Delivery(None),
            );
        };
        let started = Instant::now();
        let op = op_name(&request);
        let task = task_of(&request).map(str::to_string);
        let (response, delivery) = match self.serve(&mut state, request).await {
            Ok(done) => done,
            Err(err) => (
                Response::refused(refusal::UNAVAILABLE, format!("{err:#}")),
                Delivery(None),
            ),
        };
        // Metadata only: what was asked and how it went, never what the apps show.
        tracing::info!(
            op,
            task = task.as_deref().unwrap_or(""),
            ms = started.elapsed().as_millis() as u64,
            outcome =
                response
                    .error
                    .as_deref()
                    .unwrap_or(if response.hcu_error { "hcu_error" } else { "ok" }),
            "request"
        );
        (response, delivery)
    }

    async fn serve(&self, state: &mut State<B>, request: Request) -> Result<(Response, Delivery)> {
        self.ensure_started(state).await?;
        if let Some(task) = task_of(&request)
            && !matches!(request, Request::Screenshot { .. } | Request::Zoom { .. })
            && state.owner.as_deref() != Some(task)
        {
            // This task takes over the observation context: refs shown to
            // another task are void, and its own earlier ones too (another
            // task's reads may have moved the apps on).
            state.owner = Some(task.to_string());
            state.issued.clear();
        }
        let done = |response| Ok((response, Delivery(None)));
        match request {
            Request::Status => done(self.status(state)),
            Request::Windows { .. } => done(self.windows(state).await?),
            Request::Observe {
                window,
                root,
                read_text,
                ..
            } => done(
                self.observe(state, &window, root.as_deref(), read_text.as_ref())
                    .await?,
            ),
            Request::Find { window, query, .. } => done(self.find(state, &window, &query).await?),
            Request::Prepare { window, steps, .. } => {
                done(match self.prepare(state, &window, &steps).await? {
                    Ok(bindings) => Response {
                        ok: true,
                        bindings: Some(bindings),
                        ..Response::default()
                    },
                    Err(refused) => refused,
                })
            }
            Request::Act {
                window,
                steps,
                expect,
                ..
            } => self.act(state, &window, &steps, &expect).await,
            Request::Zoom { task, region } => done(self.zoom(state, &task, region).await?),
            Request::PrepareGui { task, actions } => {
                done(match self.prepare_gui(state, &task, &actions).await? {
                    Ok(binding) => Response {
                        ok: true,
                        gui: Some(binding),
                        ..Response::default()
                    },
                    Err(refused) => refused,
                })
            }
            Request::Gui {
                task,
                actions,
                expect,
            } => self.gui(state, &task, &actions, &expect).await,
            Request::Screenshot { task } => done(self.screenshot(state, &task).await?),
        }
    }

    async fn ensure_started(&self, state: &mut State<B>) -> Result<()> {
        if state.epoch.is_some() && state.backend.running() {
            return Ok(());
        }
        state.backend.stop();
        state.epoch = None;
        let version = state.backend.start().await?;
        state.starts += 1;
        state.epoch = Some(format!("{}-{}", self.boot, state.starts));
        // New refs: nothing from before means anything.
        state.owner = None;
        state.issued.clear();
        state.windows.clear();
        tracing::info!(
            version,
            epoch = state.epoch.as_deref().unwrap_or(""),
            "computer-use started"
        );
        Ok(())
    }

    /// A call to HCU. A stream that broke or timed out leaves HCU unusable:
    /// it's stopped, and starts again (with new refs) on the next request.
    async fn call(
        &self,
        state: &mut State<B>,
        tool: &str,
        arguments: Value,
        deadline: Duration,
    ) -> Result<ToolResult> {
        match state.backend.call(tool, arguments, deadline).await {
            Ok(result) => Ok(result),
            Err(err) => {
                state.backend.stop();
                state.epoch = None;
                Err(err.context("computer-use stopped answering; it starts again on the next request"))
            }
        }
    }

    fn status(&self, state: &mut State<B>) -> Response {
        Response {
            ok: true,
            status: Some(json!({
                "epoch": state.epoch,
                "apps": self.rules.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
                "pending_input": state.pending.as_ref().map(|p| &p.window),
            })),
            ..Response::default()
        }
    }

    /// A fresh AX `apps` listing, with each window's process and whether the
    /// agent may use it. Clears the input gate if the window it waits on is gone.
    async fn list(&self, state: &mut State<B>) -> Result<(ToolResult, Vec<OffBus>)> {
        let result = self
            .call(state, "AX", json!({"mode": "apps"}), READ_DEADLINE)
            .await?;
        let (lines, off) = parse_apps(&result.text);
        let mut windows = BTreeMap::new();
        for line in lines {
            let rule = self.rules.iter().find(|r| r.id == line.app_id).cloned();
            let (mut pid, mut exe) = (0, String::new());
            let rule = match rule {
                Some(rule) if !line.no_tree => {
                    let owners = state.backend.owners(&line.title).await.unwrap_or_default();
                    // Every window with this title must be the app's own: a
                    // title alone can't say which of two processes it means.
                    if let Some((p, e)) = owners.first().cloned()
                        && owners.iter().all(|(_, e)| exe_matches(&rule.exe, e))
                    {
                        (pid, exe) = (p, e);
                        Some(rule)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            windows.insert(line.window.clone(), Window { line, rule, pid, exe });
        }
        state.windows = windows;
        if let Some(pending) = &state.pending
            && Some(&pending.epoch) == state.epoch.as_ref()
            && !state.windows.contains_key(&pending.window)
        {
            state.pending = None;
        }
        Ok((result, off))
    }

    async fn windows(&self, state: &mut State<B>) -> Result<Response> {
        let (result, off) = self.list(state).await?;
        let mut text = vec![result.text.lines().next().unwrap_or_default().to_string()];
        for window in state.windows.values().filter(|w| w.allowed()) {
            let name = &window.rule.as_ref().expect("allowed").name;
            text.push(
                window
                    .line
                    .line
                    .replace(&format!("app={}", window.line.app_id), &format!("app={name}")),
            );
        }
        if text.len() == 1 {
            let names: Vec<&str> = self.rules.iter().map(|r| r.name.as_str()).collect();
            text.push(format!(
                "No window of an app you may use is open. The apps: {}.",
                names.join(", ")
            ));
        }
        for app in off {
            if state
                .backend
                .exe(app.pid)
                .is_some_and(|exe| self.rules.iter().any(|r| exe_matches(&r.exe, &exe)))
            {
                text.push(app.line);
            }
        }
        let text = text.join("\n");
        state.issued.extend(refs(&text));
        Ok(Response {
            ok: true,
            text: Some(text),
            hcu_error: result.is_error,
            ..Response::default()
        })
    }

    /// The window, checked: shown to this task, still open, and an app's the
    /// agent may use. Refreshes the listing.
    async fn usable(&self, state: &mut State<B>, window: &str) -> Result<Result<Window, Response>> {
        if !state.issued.contains(window) {
            return Ok(Err(Response::refused(
                refusal::STALE,
                format!("{window} isn't a window from your latest results; list the windows again"),
            )));
        }
        self.list(state).await?;
        Ok(match state.windows.get(window) {
            Some(w) if w.allowed() => Ok(w.clone()),
            Some(_) => Err(Response::refused(
                refusal::NOT_ALLOWED,
                format!("{window} isn't a window of an app you may use"),
            )),
            None => Err(Response::refused(
                refusal::STALE,
                format!("{window} is closed or was replaced; list the windows again"),
            )),
        })
    }

    fn shown(state: &State<B>, element: &str) -> Option<Response> {
        (!state.issued.contains(element)).then(|| {
            Response::refused(
                refusal::STALE,
                format!("{element} isn't from your latest results; observe or find it again"),
            )
        })
    }

    async fn observe(
        &self,
        state: &mut State<B>,
        window: &str,
        root: Option<&str>,
        read_text: Option<&TextPage>,
    ) -> Result<Response> {
        if let Err(refused) = self.usable(state, window).await? {
            return Ok(refused);
        }
        // HCU sends a window's tree once per process and then only "Tree
        // unchanged", whichever task it went to. A whole-window read is
        // always `full` (its screenshot stays here), so each task sees the tree.
        let mut arguments = json!({"mode": "observe", "window_ref": window, "observe": "full"});
        if let Some(root) = root {
            // `focused` is HCU's name for the focused element, not a ref.
            if root != "focused"
                && let Some(refused) = Self::shown(state, root)
            {
                return Ok(refused);
            }
            arguments["root"] = json!(root);
            arguments.as_object_mut().expect("an object").remove("observe");
        }
        if let Some(page) = read_text {
            if root.is_none() {
                return Ok(Response::refused(
                    refusal::INVALID,
                    "read_text needs a root element",
                ));
            }
            arguments["read_text"] = serde_json::to_value(page)?;
        }
        let result = self.call(state, "AX", arguments, READ_DEADLINE).await?;
        if root.is_none() {
            // HCU took a frame with it (`full`).
            state.frames += 1;
        }
        state.issued.extend(refs(&result.text));
        // A whole window read is the look an uncertain input waits for.
        if root.is_none()
            && !result.is_error
            && let Some(pending) = &state.pending
            && (pending.window == window || Some(&pending.epoch) != state.epoch.as_ref())
        {
            state.pending = None;
        }
        Ok(Response {
            ok: true,
            text: Some(result.text),
            hcu_error: result.is_error,
            ..Response::default()
        })
    }

    async fn find(&self, state: &mut State<B>, window: &str, query: &Query) -> Result<Response> {
        if query.is_empty() {
            return Ok(Response::refused(refusal::INVALID, "give at least one filter"));
        }
        if let Err(refused) = self.usable(state, window).await? {
            return Ok(refused);
        }
        let arguments = json!({"mode": "find", "window_ref": window, "query": query});
        let result = self.call(state, "AX", arguments, READ_DEADLINE).await?;
        state.issued.extend(refs(&result.text));
        Ok(Response {
            ok: true,
            text: Some(result.text),
            hcu_error: result.is_error,
            ..Response::default()
        })
    }

    /// Every step's target as the app has it now: what an approval shows and
    /// binds. Element steps bind their element; the others bind the window
    /// their input goes to (the batch's, or the one `activate` switched to).
    async fn prepare(
        &self,
        state: &mut State<B>,
        window: &str,
        steps: &[Step],
    ) -> Result<Result<Vec<Binding>, Response>> {
        if let Err(why) = steps::validate(steps) {
            return Ok(Err(Response::refused(refusal::INVALID, why)));
        }
        let first = match self.usable(state, window).await? {
            Ok(w) => w,
            Err(refused) => return Ok(Err(refused)),
        };
        for step in steps {
            for named in step.refs() {
                if let Some(refused) = Self::shown(state, named) {
                    return Ok(Err(refused));
                }
            }
        }
        let epoch = state.epoch.clone().unwrap_or_default();
        let mut current = (window.to_string(), first);
        let mut trees: BTreeMap<String, String> = BTreeMap::new();
        let mut bindings = Vec::with_capacity(steps.len());
        for step in steps {
            if let Step::Activate { window: other } | Step::Close { window: other } = step {
                let Some(target) = state.windows.get(other).filter(|w| w.allowed()).cloned() else {
                    return Ok(Err(Response::refused(
                        refusal::NOT_ALLOWED,
                        format!("{other} isn't a window of an app you may use"),
                    )));
                };
                bindings.push(window_binding(&epoch, other, &target));
                if matches!(step, Step::Activate { .. }) {
                    current = (other.clone(), target);
                }
                continue;
            }
            let Some(element) = step.element() else {
                bindings.push(window_binding(&epoch, &current.0, &current.1));
                continue;
            };
            if !trees.contains_key(&current.0) {
                // `full`: a repeated `tree` would come back as "Tree unchanged".
                let arguments = json!({"mode": "observe", "window_ref": current.0, "observe": "full"});
                let tree = self.call(state, "AX", arguments, READ_DEADLINE).await?;
                // HCU took that frame: a screenshot the model saw is no longer the latest.
                state.frames += 1;
                if tree.is_error {
                    return Ok(Err(Response::refused(
                        refusal::STALE,
                        format!("{} couldn't be read now; observe it again", current.0),
                    )));
                }
                trees.insert(current.0.clone(), tree.text);
            }
            let Some(found) = find_element(&trees[&current.0], element) else {
                return Ok(Err(Response::refused(
                    refusal::STALE,
                    format!("{element} isn't in {} now; observe it again", current.0),
                )));
            };
            if step.text().is_some() && found.role.contains("password") {
                return Ok(Err(Response::refused(
                    refusal::NOT_ALLOWED,
                    format!("{element} is a password field: the user types passwords (Take over)"),
                )));
            }
            let w = &current.1;
            let rule = w.rule.clone().expect("usable windows have a rule");
            bindings.push(Binding {
                epoch: epoch.clone(),
                window: current.0.clone(),
                element: element.to_string(),
                app: rule.name,
                app_id: w.line.app_id.clone(),
                pid: w.pid,
                exe: w.exe.clone(),
                title: w.line.title.clone(),
                role: found.role,
                name: found.name,
                path: found.path,
            });
        }
        Ok(Ok(bindings))
    }

    /// Why no input may be sent now, if an earlier one's outcome is unknown.
    fn gate(state: &State<B>) -> Option<String> {
        let pending = state.pending.as_ref()?;
        let which = if pending.window == DESKTOP {
            "the desktop (take a screenshot)".to_string()
        } else if Some(&pending.epoch) == state.epoch.as_ref() {
            format!("window {}", pending.window)
        } else {
            "the app's window (list the windows: refs changed)".to_string()
        };
        Some(format!(
            "an earlier input's outcome is unknown; look at {which} before any more input"
        ))
    }

    /// Input whose outcome is unknown until its result reaches agentd.
    fn begin_input(state: &mut State<B>, window: &str) -> u64 {
        state.tokens += 1;
        let token = state.tokens;
        state.pending = Some(Pending {
            epoch: state.epoch.clone().unwrap_or_default(),
            window: window.to_string(),
            token,
        });
        token
    }

    async fn act(
        &self,
        state: &mut State<B>,
        window: &str,
        steps: &[Step],
        expect: &[Binding],
    ) -> Result<(Response, Delivery)> {
        let refused = |code, message: String| Ok((Response::refused(code, message), Delivery(None)));
        if let Some(why) = Self::gate(state) {
            return refused(refusal::INSPECT_FIRST, why);
        }
        if expect.first().map(|b| b.epoch.as_str()) != state.epoch.as_deref() {
            return refused(
                refusal::CHANGED,
                "computer-use restarted since this was approved; its refs are gone".into(),
            );
        }
        let now = match self.prepare(state, window, steps).await? {
            Ok(bindings) => bindings,
            Err(refused) => return Ok((refused, Delivery(None))),
        };
        if now != expect {
            return refused(
                refusal::CHANGED,
                "the app isn't what was approved any more (a window or an element changed)".into(),
            );
        }
        let token = Self::begin_input(state, window);
        let waits: u64 = steps
            .iter()
            .map(|step| match step {
                Step::Wait(wait) => wait.timeout_ms.unwrap_or(5000),
                _ => 0,
            })
            .sum();
        let arguments = json!({
            "mode": "act",
            "window_ref": window,
            "actions": steps.iter().map(Step::hcu).collect::<Vec<_>>(),
            "observe": "diff",
        });
        let deadline = ACT_DEADLINE + Duration::from_millis(waits);
        let result = match self.call(state, "AX", arguments, deadline).await {
            Ok(result) => result,
            Err(err) => {
                return refused(
                    refusal::UNAVAILABLE,
                    format!(
                        "{err:#}. The input may or may not have happened: list the windows and observe before anything else"
                    ),
                );
            }
        };
        state.issued.extend(refs(&result.text));
        let uncertain = input_uncertain(evidence(&result.text).as_ref());
        Ok((
            Response {
                ok: true,
                text: Some(result.text),
                hcu_error: result.is_error,
                ..Response::default()
            },
            Delivery((!uncertain).then_some(token)),
        ))
    }

    /// The task's screenshot, if it's still HCU's latest frame.
    fn model_frame<'a>(state: &'a State<B>, task: &str) -> Option<&'a ModelFrame> {
        state
            .model_frame
            .as_ref()
            .filter(|frame| frame.task == task && frame.seq == state.frames)
    }

    async fn screenshot(&self, state: &mut State<B>, task: &str) -> Result<Response> {
        let result = self
            .call(
                state,
                "GUI",
                json!({"actions": [{"type": "screenshot"}]}),
                READ_DEADLINE,
            )
            .await?;
        state.frames += 1;
        let size = screen_size(&result.text);
        let image = result
            .images
            .into_iter()
            .next()
            .map(|(mime, data)| Image { mime, data });
        if let (Some(size), Some(_)) = (size, &image) {
            state.model_frame = Some(ModelFrame {
                task: task.to_string(),
                seq: state.frames,
                size,
            });
            // The look an uncertain pixel input waits for.
            if state.pending.as_ref().is_some_and(|p| p.window == DESKTOP) {
                state.pending = None;
            }
        }
        Ok(Response {
            ok: image.is_some(),
            error: image.is_none().then(|| refusal::UNAVAILABLE.to_string()),
            message: image.is_none().then(|| "no screenshot came back".to_string()),
            frame: size,
            image,
            ..Response::default()
        })
    }

    async fn zoom(&self, state: &mut State<B>, task: &str, region: [f64; 4]) -> Result<Response> {
        if Self::model_frame(state, task).is_none() {
            return Ok(Response::refused(refusal::STALE, NEED_SCREENSHOT));
        }
        let arguments = json!({"actions": [{"type": "zoom", "region": region}]});
        let result = self.call(state, "GUI", arguments, READ_DEADLINE).await?;
        let image = result
            .images
            .into_iter()
            .next()
            .map(|(mime, data)| Image { mime, data });
        Ok(Response {
            ok: true,
            text: Some(result.text),
            hcu_error: result.is_error,
            image,
            ..Response::default()
        })
    }

    /// Where each pixel action lands: the window under each point (scaled from
    /// the screenshot to the screen) and, for keys and text, the window that
    /// takes them (the last one clicked in the batch, or the focused one). All
    /// of them must be the apps the agent may use.
    async fn prepare_gui(
        &self,
        state: &mut State<B>,
        task: &str,
        actions: &[GuiAction],
    ) -> Result<Result<GuiBinding, Response>> {
        let Some(frame) = Self::model_frame(state, task).cloned() else {
            return Ok(Err(Response::refused(refusal::STALE, NEED_SCREENSHOT)));
        };
        if let Err(why) = steps::validate_gui(actions, frame.size) {
            return Ok(Err(Response::refused(refusal::INVALID, why)));
        }
        let screen = state.backend.screen_size().await?;
        let mut targets = Vec::new();
        let mut keyboard: Option<GuiTarget> = None;
        for (i, action) in actions.iter().enumerate() {
            let points = action.points();
            if points.is_empty() {
                if matches!(action, GuiAction::Wait) {
                    continue;
                }
                let target = match &keyboard {
                    Some(clicked) => GuiTarget {
                        action: i,
                        at: None,
                        ..clicked.clone()
                    },
                    None => {
                        let Some((title, pid)) = state.backend.active_window().await? else {
                            return Ok(Err(Response::refused(
                                refusal::NOT_ALLOWED,
                                "no window has the keyboard: click into the app's window first",
                            )));
                        };
                        match self.gui_target(state, i, None, title, pid) {
                            Ok(target) => target,
                            Err(why) => return Ok(Err(Response::refused(refusal::NOT_ALLOWED, why))),
                        }
                    }
                };
                targets.push(target);
                continue;
            }
            for point in points {
                let x = (point.x * screen.0 as f64 / frame.size.0 as f64).floor() as i64;
                let y = (point.y * screen.1 as f64 / frame.size.1 as f64).floor() as i64;
                let Some((title, pid)) = state.backend.window_at(x, y).await? else {
                    return Ok(Err(Response::refused(
                        refusal::NOT_ALLOWED,
                        format!("({}, {}) isn't on any window", point.x, point.y),
                    )));
                };
                let target = match self.gui_target(state, i, Some((point.x, point.y)), title, pid) {
                    Ok(target) => target,
                    Err(why) => return Ok(Err(Response::refused(refusal::NOT_ALLOWED, why))),
                };
                if matches!(action, GuiAction::Click { .. } | GuiAction::DoubleClick { .. }) {
                    keyboard = Some(target.clone());
                }
                targets.push(target);
            }
        }
        Ok(Ok(GuiBinding {
            epoch: state.epoch.clone().unwrap_or_default(),
            frame: frame.seq,
            size: frame.size,
            targets,
        }))
    }

    fn gui_target(
        &self,
        state: &mut State<B>,
        action: usize,
        at: Option<(f64, f64)>,
        title: String,
        pid: u32,
    ) -> Result<GuiTarget, String> {
        let exe = state.backend.exe(pid).unwrap_or_default();
        let Some(rule) = self.rules.iter().find(|r| exe_matches(&r.exe, &exe)) else {
            let place = at.map_or("the keyboard".to_string(), |(x, y)| format!("({x}, {y})"));
            return Err(format!("{place} is on “{title}”, not an app you may use"));
        };
        Ok(GuiTarget {
            action,
            at,
            app: rule.name.clone(),
            title,
            pid,
            exe,
        })
    }

    async fn gui(
        &self,
        state: &mut State<B>,
        task: &str,
        actions: &[GuiAction],
        expect: &GuiBinding,
    ) -> Result<(Response, Delivery)> {
        let refused = |code, message: String| Ok((Response::refused(code, message), Delivery(None)));
        if let Some(why) = Self::gate(state) {
            return refused(refusal::INSPECT_FIRST, why);
        }
        let now = match self.prepare_gui(state, task, actions).await? {
            Ok(binding) => binding,
            Err(refused) => return Ok((refused, Delivery(None))),
        };
        if &now != expect {
            return refused(
                refusal::CHANGED,
                "the desktop isn't what was approved any more (a window moved, or another one is in the way)"
                    .into(),
            );
        }
        let token = Self::begin_input(state, DESKTOP);
        let waits = actions.iter().filter(|a| matches!(a, GuiAction::Wait)).count() as u64;
        let arguments = json!({"actions": actions.iter().map(GuiAction::hcu).collect::<Vec<_>>()});
        let deadline = ACT_DEADLINE + Duration::from_secs(waits);
        let result = match self.call(state, "GUI", arguments, deadline).await {
            Ok(result) => result,
            Err(err) => {
                return refused(
                    refusal::UNAVAILABLE,
                    format!(
                        "{err:#}. The input may or may not have happened: take a screenshot before anything else"
                    ),
                );
            }
        };
        // HCU's final screenshot (or a fresh one, if it refused) is the new frame.
        state.frames += 1;
        let size = screen_size(&result.text);
        let image = result
            .images
            .into_iter()
            .next()
            .map(|(mime, data)| Image { mime, data });
        if let (Some(size), Some(_)) = (size, &image) {
            state.model_frame = Some(ModelFrame {
                task: task.to_string(),
                seq: state.frames,
                size,
            });
        }
        // The screenshot it returns is the look an unclear outcome needs.
        let seen = image.is_some() || !input_uncertain(evidence(&result.text).as_ref());
        Ok((
            Response {
                ok: true,
                text: Some(result.text),
                hcu_error: result.is_error,
                frame: size,
                image,
                ..Response::default()
            },
            Delivery(seen.then_some(token)),
        ))
    }
}

/// A window step's binding: the window and its app, no element.
fn window_binding(epoch: &str, window: &str, w: &Window) -> Binding {
    Binding {
        epoch: epoch.to_string(),
        window: window.to_string(),
        element: String::new(),
        app: w.rule.as_ref().map(|r| r.name.clone()).unwrap_or_default(),
        app_id: w.line.app_id.clone(),
        pid: w.pid,
        exe: w.exe.clone(),
        title: w.line.title.clone(),
        role: "window".into(),
        name: w.line.title.clone(),
        path: String::new(),
    }
}

/// A GUI result's image size, from its evidence line's `screen`.
fn screen_size(text: &str) -> Option<(u32, u32)> {
    let e = evidence(text)?;
    Some((
        e["screen"]["width"].as_u64()? as u32,
        e["screen"]["height"].as_u64()? as u32,
    ))
}

fn op_name(request: &Request) -> &'static str {
    match request {
        Request::Status => "status",
        Request::Windows { .. } => "windows",
        Request::Observe { .. } => "observe",
        Request::Find { .. } => "find",
        Request::Prepare { .. } => "prepare",
        Request::Act { .. } => "act",
        Request::Screenshot { .. } => "screenshot",
        Request::Zoom { .. } => "zoom",
        Request::PrepareGui { .. } => "prepare_gui",
        Request::Gui { .. } => "gui",
    }
}

fn task_of(request: &Request) -> Option<&str> {
    match request {
        Request::Status => None,
        Request::Windows { task }
        | Request::Observe { task, .. }
        | Request::Find { task, .. }
        | Request::Prepare { task, .. }
        | Request::Act { task, .. }
        | Request::Screenshot { task }
        | Request::Zoom { task, .. }
        | Request::PrepareGui { task, .. }
        | Request::Gui { task, .. } => Some(task),
    }
}

// --- The live desktop ------------------------------------------------------------------

pub struct Live {
    spawn: Spawn,
    hcu: Option<Hcu>,
    stderr: Arc<StdMutex<Vec<String>>>,
}

impl Live {
    pub fn new(spawn: Spawn) -> Self {
        Self {
            spawn,
            hcu: None,
            stderr: Arc::default(),
        }
    }

    /// XFCE must be running on the bus HCU gets: a session bus of its own
    /// (dbus-launch's) would hide every app from HCU.
    fn check_session(&self) -> Result<()> {
        let ours = self
            .spawn
            .env
            .get("DBUS_SESSION_BUS_ADDRESS")
            .cloned()
            .unwrap_or_default();
        let uid = std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m))?;
        for entry in std::fs::read_dir("/proc")?.flatten() {
            let path = entry.path();
            let Ok(comm) = std::fs::read_to_string(path.join("comm")) else {
                continue;
            };
            if comm.trim() != "xfce4-session"
                || std::fs::metadata(&path)
                    .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
                    .ok()
                    != Some(uid)
            {
                continue;
            }
            let environ = std::fs::read(path.join("environ")).unwrap_or_default();
            let theirs = environ
                .split(|b| *b == 0)
                .find_map(|var| var.strip_prefix(b"DBUS_SESSION_BUS_ADDRESS="))
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .unwrap_or_default();
            if theirs != ours {
                bail!("XFCE runs on another session bus ({theirs:?}, not {ours:?}): restart the xfce unit");
            }
            return Ok(());
        }
        bail!("the XFCE session isn't running")
    }
}

impl Backend for Live {
    async fn start(&mut self) -> Result<String> {
        self.check_session()?;
        self.stderr.lock().unwrap().clear();
        match Hcu::start(&self.spawn, self.stderr.clone()).await {
            Ok(hcu) => {
                let version = hcu.version.clone();
                self.hcu = Some(hcu);
                Ok(version)
            }
            Err(err) => {
                // Startup trouble is about the session, not the apps: fine to log.
                let tail = self.stderr.lock().unwrap().join(" | ");
                tracing::warn!(stderr = %tail, "computer-use didn't start");
                Err(err)
            }
        }
    }

    fn running(&mut self) -> bool {
        self.hcu.as_mut().is_some_and(Hcu::running)
    }

    fn stop(&mut self) {
        // kill_on_drop: the process goes with it.
        self.hcu = None;
    }

    async fn call(&mut self, tool: &str, arguments: Value, deadline: Duration) -> Result<ToolResult> {
        let Some(hcu) = self.hcu.as_mut() else {
            bail!("computer-use isn't running")
        };
        hcu.call(tool, arguments, deadline).await
    }

    async fn owners(&mut self, title: &str) -> Result<Vec<(u32, String)>> {
        let mut command = tokio::process::Command::new("wmctrl");
        command.arg("-lp").env_clear();
        for key in ["DISPLAY", "XAUTHORITY", "HOME"] {
            if let Some(value) = self.spawn.env.get(key) {
                command.env(key, value);
            }
        }
        let output = tokio::time::timeout(READ_DEADLINE, command.output()).await??;
        if !output.status.success() {
            bail!("wmctrl failed");
        }
        Ok(wmctrl_owners(&String::from_utf8_lossy(&output.stdout), title)
            .into_iter()
            .map(|pid| (pid, self.exe(pid).unwrap_or_default()))
            .collect())
    }

    async fn window_at(&mut self, x: i64, y: i64) -> Result<Option<(String, u32)>> {
        // Top-level windows, bottom to top: the first from the top that's
        // shown and covers the point. (Minimized windows and other workspaces'
        // aren't mapped.)
        let stacking = self.x11("xprop", &["-root", "_NET_CLIENT_LIST_STACKING"]).await?;
        for id in parse_stacking(&stacking).into_iter().rev().take(64) {
            let Ok(info) = self.x11("xwininfo", &["-id", &id]).await else {
                continue;
            };
            let Some((left, top, width, height, shown)) = parse_xwininfo(&info) else {
                continue;
            };
            if shown && x >= left && y >= top && x < left + width && y < top + height {
                return self.describe_window(&id).await.map(Some);
            }
        }
        Ok(None)
    }

    async fn active_window(&mut self) -> Result<Option<(String, u32)>> {
        let Ok(id) = self.x11("xdotool", &["getactivewindow"]).await else {
            return Ok(None);
        };
        let id = id.trim().to_string();
        if id.is_empty() {
            return Ok(None);
        }
        self.describe_window(&id).await.map(Some)
    }

    async fn screen_size(&mut self) -> Result<(u32, u32)> {
        let size = self.x11("xdotool", &["getdisplaygeometry"]).await?;
        let mut parts = size.split_whitespace().filter_map(|n| n.parse().ok());
        match (parts.next(), parts.next()) {
            (Some(w), Some(h)) => Ok((w, h)),
            _ => bail!("xdotool gave no display size"),
        }
    }

    fn exe(&mut self, pid: u32) -> Option<String> {
        (pid > 0)
            .then(|| std::fs::read_link(format!("/proc/{pid}/exe")).ok())
            .flatten()
            .map(|p| p.to_string_lossy().into_owned())
    }
}

/// Whether an executable is the one a rule names. `*` stands for one path
/// segment, so a snap's app keeps matching across its revisions
/// (`/snap/slack/*/usr/lib/slack/slack`).
fn exe_matches(pattern: &str, exe: &str) -> bool {
    let (pattern, exe): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), exe.split('/').collect());
    pattern.len() == exe.len()
        && pattern
            .iter()
            .zip(&exe)
            .all(|(p, e)| *p == *e || (*p == "*" && !e.is_empty() && *e != "." && *e != ".."))
}

impl Live {
    /// An X tool on the session's display.
    async fn x11(&self, program: &str, args: &[&str]) -> Result<String> {
        let mut command = tokio::process::Command::new(format!("/usr/bin/{program}"));
        command.args(args).env_clear();
        for key in ["DISPLAY", "XAUTHORITY", "HOME"] {
            if let Some(value) = self.spawn.env.get(key) {
                command.env(key, value);
            }
        }
        let output = tokio::time::timeout(READ_DEADLINE, command.output()).await??;
        if !output.status.success() {
            bail!("{program} failed");
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// A window's title and process.
    async fn describe_window(&self, id: &str) -> Result<(String, u32)> {
        let pid = self
            .x11("xdotool", &["getwindowpid", id])
            .await
            .unwrap_or_default();
        let title = self
            .x11("xdotool", &["getwindowname", id])
            .await
            .unwrap_or_default();
        Ok((
            title.trim_end_matches('\n').to_string(),
            pid.trim().parse().unwrap_or(0),
        ))
    }
}

/// `xprop -root _NET_CLIENT_LIST_STACKING`'s window ids, bottom to top.
fn parse_stacking(text: &str) -> Vec<String> {
    text.split_once('#')
        .map(|(_, ids)| ids)
        .unwrap_or_default()
        .split(',')
        .map(|id| id.trim().to_string())
        .filter(|id| id.starts_with("0x"))
        .collect()
}

/// `xwininfo`'s absolute position, size and whether it's shown.
fn parse_xwininfo(text: &str) -> Option<(i64, i64, i64, i64, bool)> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(name))
            .and_then(|v| v.trim().parse::<i64>().ok())
    };
    Some((
        field("Absolute upper-left X:")?,
        field("Absolute upper-left Y:")?,
        field("Width:")?,
        field("Height:")?,
        text.contains("Map State: IsViewable"),
    ))
}

/// The pids of `wmctrl -lp`'s windows with this title (HCU may clip a long
/// one with an ellipsis: then its start).
fn wmctrl_owners(listing: &str, title: &str) -> Vec<u32> {
    use std::sync::LazyLock;
    static LINE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^0x[0-9a-fA-F]+\s+-?[0-9]+\s+([0-9]+)\s+\S+ (.*)$").expect("valid regex")
    });
    let clipped = title.strip_suffix('…');
    listing
        .lines()
        .filter_map(|line| LINE.captures(line))
        .filter(|c| match clipped {
            Some(start) => c[2].starts_with(start),
            None => &c[2] == title,
        })
        .filter_map(|c| c[1].parse().ok())
        .collect()
}

// --- The process ----------------------------------------------------------------------

/// `agentd desktop-broker [--config PATH]`.
pub async fn main(args: &[String]) -> Result<()> {
    let path = match args {
        [flag, path] if flag == "--config" => PathBuf::from(path),
        [] => PathBuf::from("/etc/neo-desktop/broker.toml"),
        _ => bail!("usage: agentd desktop-broker [--config PATH]"),
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config: Config = toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    let client = uid_of(&config.client_user)?;
    let listener = listener(config.socket.as_deref())?;
    let spawn = Spawn {
        program: config.computer_use.clone(),
        args: vec!["mcp".into()],
        env: config.env.clone(),
        schemas: config.schema_sha256.clone(),
    };
    let broker = Arc::new(Broker::new(Live::new(spawn), config.apps.clone()));
    tracing::info!(apps = config.apps.len(), client = %config.client_user, "desktop broker listening");
    // HCU switches the session's accessibility on with its first request, and
    // apps started after that are visible. So start it now, and keep trying
    // while XFCE comes up after a boot.
    tokio::spawn(warm_up(broker.clone()));
    loop {
        let (stream, _) = listener.accept().await?;
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == client => {}
            Ok(cred) => {
                tracing::warn!(uid = cred.uid(), "refused a connection from another user");
                continue;
            }
            Err(err) => {
                tracing::warn!(%err, "couldn't identify a connection's user");
                continue;
            }
        }
        tokio::spawn(serve_connection(broker.clone(), stream));
    }
}

async fn warm_up<B: Backend>(broker: Arc<Broker<B>>) {
    let mut wait = Duration::from_secs(2);
    loop {
        let (response, _) = broker
            .handle(Request::Windows {
                task: "broker".into(),
            })
            .await;
        if response.ok {
            return;
        }
        tracing::info!(
            reason = response.message.as_deref().unwrap_or(""),
            "the desktop isn't ready for computer-use yet"
        );
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(60));
    }
}

pub async fn serve_connection<B: Backend>(broker: Arc<Broker<B>>, stream: tokio::net::UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut line = Vec::new();
    let read = BufReader::new(read)
        .take(MAX_REQUEST)
        .read_until(b'\n', &mut line)
        .await;
    let request = match read {
        Ok(n) if n > 0 => serde_json::from_slice::<Request>(&line),
        _ => return,
    };
    let (response, delivery) = match request {
        Ok(request) => broker.handle(request).await,
        Err(err) => (
            Response::refused(refusal::INVALID, format!("bad request: {err}")),
            Delivery(None),
        ),
    };
    let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
    bytes.push(b'\n');
    if write.write_all(&bytes).await.is_ok() && write.flush().await.is_ok() {
        broker.delivered(delivery).await;
    }
}

/// systemd's socket (LISTEN_FDS), or one bound here for a local run.
fn listener(socket: Option<&Path>) -> Result<tokio::net::UnixListener> {
    let ours = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id());
    if ours && std::env::var("LISTEN_FDS").as_deref() == Ok("1") {
        // SAFETY: systemd passed exactly one socket, as fd 3, to this process.
        let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
        listener.set_nonblocking(true)?;
        return Ok(tokio::net::UnixListener::from_std(listener)?);
    }
    let Some(path) = socket else {
        bail!("no socket: run under systemd's neo-desktop-broker.socket, or set socket in the config")
    };
    let _ = std::fs::remove_file(path);
    let listener =
        tokio::net::UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn uid_of(user: &str) -> Result<u32> {
    let passwd = std::fs::read_to_string("/etc/passwd")?;
    passwd
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            (fields.next() == Some(user)).then(|| fields.nth(1)?.parse().ok())?
        })
        .with_context(|| format!("no user {user}"))
}

#[cfg(test)]
mod tests {
    use super::super::hcu::fake;
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// What HCU was asked, and the script it answers from.
    type Calls = Arc<StdMutex<Vec<Value>>>;
    type Shared = Arc<StdMutex<Script>>;

    /// A desktop with one Mousepad window (w1), a terminal (w2) and Chrome
    /// (w3). On screen (1280×800, shown at 1229×768): Mousepad's window at
    /// 10..650 × 30..540, the terminal at 700..1200 × 30..540, the desktop
    /// behind them.
    struct Fake {
        session: Option<fake::Pipes>,
        calls: Calls,
        script: Shared,
        starts: u32,
    }

    #[derive(Default)]
    struct Script {
        /// The File menu is open (its items are in the tree).
        menu_open: bool,
        /// What the next act returns (default: a clean success).
        click: Option<String>,
        /// The editor's own title.
        title: String,
        /// Acts hang (the stream breaks mid-call).
        hang: bool,
        /// The focused window's process.
        focused: u32,
        /// Mousepad's window moved right (under another point).
        moved: bool,
    }

    const MOUSEPAD: &str = "/usr/bin/mousepad";
    const SCREEN: &str =
        "\"screen\":{\"coordinate_space\":\"screen_image_px\",\"height\":768,\"width\":1229}";

    fn tree(script: &Script) -> String {
        let items = if script.menu_open {
            "   - menuitem \"Save\" [ref=r13]:\n    - /description: Save the current document\n"
        } else {
            ""
        };
        format!(
            "{{\"ok\":true,\"input_emitted\":false}}\napp: \"org.xfce.mousepad\"\nwindow: \"{}\" [ref=w1]\nax:\n - window \"{}\":\n  - menu \"File\" [ref=r1]:\n   - /description: File menu\n{items}  - textbox [ref=r7]:\n  - password text \"Passphrase\" [ref=r20]:\n",
            script.title, script.title
        )
    }

    fn picture(text: String) -> Value {
        json!({"content": [
            {"type": "text", "text": text},
            {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="},
        ], "isError": false})
    }

    impl Fake {
        fn new() -> (Self, Calls, Shared) {
            let calls = Arc::new(StdMutex::new(Vec::new()));
            let script = Arc::new(StdMutex::new(Script {
                title: "notes.txt - Mousepad".into(),
                focused: 41,
                ..Script::default()
            }));
            (
                Self {
                    session: None,
                    calls: calls.clone(),
                    script: script.clone(),
                    starts: 0,
                },
                calls,
                script,
            )
        }
    }

    impl Backend for Fake {
        async fn start(&mut self) -> Result<String> {
            let calls = self.calls.clone();
            let script = self.script.clone();
            let mut session = fake::session(move |tool, args| {
                calls.lock().unwrap().push(json!({"tool": tool, "args": args}));
                let mut s = script.lock().unwrap();
                match (tool, args["mode"].as_str().unwrap_or_default()) {
                    ("AX", "apps") => fake::text(
                        &format!(
                            "{{\"ok\":true,\"input_emitted\":false}}\nw1 \"{}\"  app=org.xfce.mousepad  active\nw2 \"Terminal\"  app=xfce4-terminal\nw3 \"Inbox - Google Chrome\"  app=chrome  (no accessibility tree: use GUI)\nNot on the accessibility bus: \"Mousepad\" (pid 41, GTK). restart it.\nNot on the accessibility bus: \"Google Chrome\" (pid 50, Chromium). restart it.",
                            s.title
                        ),
                        false,
                    ),
                    ("AX", "observe") | ("AX", "find") => fake::text(&tree(&s), false),
                    ("AX", "act") => {
                        let element = args["actions"][0]["ref"].as_str().unwrap_or_default().to_string();
                        if element == "r1" {
                            s.menu_open = true;
                        }
                        let text = s.click.take().unwrap_or_else(|| {
                            "{\"ok\":true,\"input_emitted\":true,\"completed\":1,\"requested\":1}\neffect: r1 menu \"File\" selected=false→true\n - menuitem \"Save\" [ref=r13]:".into()
                        });
                        fake::text(&text, false)
                    }
                    ("GUI", _) if args["actions"][0]["type"] == "zoom" => {
                        picture("{\"ok\":true,\"input_emitted\":false,\"zoom\":{}}".into())
                    }
                    ("GUI", _) if args["actions"][0]["type"] == "screenshot" => {
                        picture(format!("{{\"ok\":true,\"input_emitted\":false,{SCREEN}}}"))
                    }
                    ("GUI", _) => picture(format!(
                        "{{\"ok\":true,\"input_emitted\":true,\"completed\":1,\"requested\":1,{SCREEN}}}"
                    )),
                    _ => fake::text("{\"ok\":false,\"code\":\"invalid_argument\"}", true),
                }
            });
            session.initialize(&BTreeMap::new()).await?;
            self.session = Some(session);
            self.starts += 1;
            Ok("1.0.1".into())
        }

        fn running(&mut self) -> bool {
            self.session.is_some()
        }

        fn stop(&mut self) {
            self.session = None;
        }

        async fn call(&mut self, tool: &str, arguments: Value, deadline: Duration) -> Result<ToolResult> {
            if self.script.lock().unwrap().hang && arguments["mode"] == "act" {
                bail!("computer-use didn't answer within {}s", deadline.as_secs());
            }
            self.session
                .as_mut()
                .expect("started")
                .call(tool, arguments)
                .await
        }

        async fn owners(&mut self, title: &str) -> Result<Vec<(u32, String)>> {
            Ok(match title {
                "Terminal" => vec![(42, "/usr/bin/xfce4-terminal".into())],
                t if t.ends_with("Mousepad") => vec![(41, MOUSEPAD.into())],
                _ => vec![],
            })
        }

        fn exe(&mut self, pid: u32) -> Option<String> {
            match pid {
                41 => Some(MOUSEPAD.into()),
                42 => Some("/usr/bin/xfce4-terminal".into()),
                50 => Some("/opt/google/chrome/chrome".into()),
                99 => Some("/usr/bin/xfdesktop".into()),
                _ => None,
            }
        }

        async fn window_at(&mut self, x: i64, y: i64) -> Result<Option<(String, u32)>> {
            let s = self.script.lock().unwrap();
            let shift = if s.moved { 600 } else { 0 };
            Ok(Some(
                if (10 + shift..650 + shift).contains(&x) && (30..540).contains(&y) {
                    (s.title.clone(), 41)
                } else if (700..1200).contains(&x) && (30..540).contains(&y) {
                    ("Terminal".into(), 42)
                } else {
                    ("Desktop".into(), 99)
                },
            ))
        }

        async fn active_window(&mut self) -> Result<Option<(String, u32)>> {
            let s = self.script.lock().unwrap();
            Ok(Some(match s.focused {
                41 => (s.title.clone(), 41),
                pid => ("Terminal".into(), pid),
            }))
        }

        async fn screen_size(&mut self) -> Result<(u32, u32)> {
            Ok((1280, 800))
        }
    }

    fn rules() -> Vec<AppRule> {
        vec![
            AppRule {
                id: "org.xfce.mousepad".into(),
                exe: MOUSEPAD.into(),
                name: "Mousepad".into(),
            },
            // A terminal listed by id but running another executable: still refused.
            AppRule {
                id: "xfce4-terminal".into(),
                exe: "/usr/bin/not-the-terminal".into(),
                name: "Terminal".into(),
            },
        ]
    }

    fn broker() -> (Broker<Fake>, Calls, Shared) {
        let (fake, calls, script) = Fake::new();
        (Broker::new(fake, rules()), calls, script)
    }

    async fn ask(broker: &Broker<Fake>, request: Request) -> Response {
        let (response, delivery) = broker.handle(request).await;
        broker.delivered(delivery).await;
        response
    }

    fn windows(task: &str) -> Request {
        Request::Windows { task: task.into() }
    }

    fn observe(task: &str, window: &str, root: Option<&str>) -> Request {
        Request::Observe {
            task: task.into(),
            window: window.into(),
            root: root.map(String::from),
            read_text: None,
        }
    }

    fn steps(value: Value) -> Vec<Step> {
        serde_json::from_value(value).unwrap()
    }

    fn click_file() -> Vec<Step> {
        steps(json!([{"type": "click", "ref": "r1"}]))
    }

    fn prepare(task: &str, steps: &[Step]) -> Request {
        Request::Prepare {
            task: task.into(),
            window: "w1".into(),
            steps: steps.to_vec(),
        }
    }

    fn act(task: &str, steps: &[Step], expect: &[Binding]) -> Request {
        Request::Act {
            task: task.into(),
            window: "w1".into(),
            steps: steps.to_vec(),
            expect: expect.to_vec(),
        }
    }

    fn calls_of(calls: &Calls, tool: &str, kind: &str) -> Vec<Value> {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| {
                c["tool"] == tool && (c["args"]["mode"] == kind || c["args"]["actions"][0]["type"] == kind)
            })
            .cloned()
            .collect()
    }

    fn acts(calls: &Calls) -> Vec<Value> {
        calls_of(calls, "AX", "act")
    }

    /// A task that listed and read Mousepad's window.
    async fn looked(broker: &Broker<Fake>, task: &str) {
        ask(broker, windows(task)).await;
        ask(broker, observe(task, "w1", None)).await;
    }

    async fn bindings(broker: &Broker<Fake>, task: &str, batch: &[Step]) -> Vec<Binding> {
        let response = ask(broker, prepare(task, batch)).await;
        response
            .bindings
            .clone()
            .unwrap_or_else(|| panic!("{response:?}"))
    }

    #[tokio::test]
    async fn lists_only_the_apps_the_agent_may_use() {
        let (broker, _, _) = broker();
        let response = ask(&broker, windows("t1")).await;
        let text = response.text.unwrap();
        assert!(text.starts_with("{\"ok\":true"), "{text}");
        assert!(
            text.contains("w1 \"notes.txt - Mousepad\"  app=Mousepad  active"),
            "{text}"
        );
        // Not the terminal (its executable isn't the listed one), not Chrome.
        assert!(!text.contains("Terminal") && !text.contains("Chrome"), "{text}");
        assert!(text.contains("\"Mousepad\" (pid 41"), "{text}");
        // Its windows: other refs refused before HCU is asked.
        assert_eq!(
            ask(&broker, observe("t1", "w2", None)).await.error.as_deref(),
            Some(refusal::STALE)
        );
    }

    #[tokio::test]
    async fn a_window_of_another_app_is_refused_even_when_named() {
        let (first, _, _) = broker();
        ask(&first, windows("t1")).await;
        // The model guesses a ref it wasn't given…
        let response = ask(&first, observe("t1", "w3", None)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::STALE));
        // …or one that shows up in text it was given: the listing still decides.
        let (second, _, script) = broker();
        script.lock().unwrap().title = "see w2 - Mousepad".into();
        ask(&second, windows("t1")).await;
        let response = ask(&second, observe("t1", "w2", None)).await;
        assert_eq!(
            response.error.as_deref(),
            Some(refusal::NOT_ALLOWED),
            "{response:?}"
        );
        // Nor can a batch switch to it.
        ask(&second, observe("t1", "w1", None)).await;
        let switch =
            steps(json!([{"type": "activate", "window": "w2"}, {"type": "keypress", "keys": ["ENTER"]}]));
        let response = ask(&second, prepare("t1", &switch)).await;
        assert_eq!(
            response.error.as_deref(),
            Some(refusal::NOT_ALLOWED),
            "{response:?}"
        );
    }

    #[tokio::test]
    async fn an_approved_batch_goes_ahead_only_on_the_same_targets() {
        let (broker, calls, script) = broker();
        looked(&broker, "t1").await;
        let batch = steps(json!([
            {"type": "click", "ref": "r1"},
            {"type": "wait", "settle": true, "timeout_ms": 2000},
            {"type": "type", "ref": "r7", "text": "milk\nbread ✓"},
            {"type": "keypress", "keys": ["CTRL", "S"]},
        ]));
        let expect = bindings(&broker, "t1", &batch).await;
        assert_eq!(expect.len(), 4);
        assert_eq!(
            (expect[0].role.as_str(), expect[0].name.as_str()),
            ("menu", "File")
        );
        assert_eq!(expect[0].path, "menu “File” — File menu");
        assert_eq!((expect[0].pid, expect[0].exe.as_str()), (41, MOUSEPAD));
        assert_eq!(expect[2].role, "textbox");
        // Keys and waits bind the window they go to.
        assert_eq!(
            (expect[3].role.as_str(), expect[3].title.as_str()),
            ("window", "notes.txt - Mousepad")
        );
        // The window's title changed while the user decided: refused.
        script.lock().unwrap().title = "other.txt - Mousepad".into();
        let response = ask(&broker, act("t1", &batch, &expect)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::CHANGED));
        assert!(acts(&calls).is_empty());
        // As approved: the whole batch, in HCU's form, in order.
        script.lock().unwrap().title = "notes.txt - Mousepad".into();
        let response = ask(&broker, act("t1", &batch, &expect)).await;
        assert!(response.ok && !response.hcu_error, "{response:?}");
        let sent = &acts(&calls)[0]["args"];
        assert_eq!(sent["window_ref"], "w1");
        assert_eq!(
            sent["actions"][0],
            json!({"type": "click", "ref": "r1", "allow_global_input": false})
        );
        assert_eq!(
            sent["actions"][1],
            json!({"type": "wait", "settle": true, "timeout_ms": 2000})
        );
        assert_eq!(sent["actions"][2]["text"], "milk\nbread ✓");
        assert_eq!(sent["actions"][3]["keys"], json!(["CTRL", "S"]));
        // Whole-window reads ask for the full tree: HCU would elide a repeat.
        let observes = calls_of(&calls, "AX", "observe");
        assert!(
            observes.iter().all(|c| c["args"]["observe"] == "full"),
            "{observes:?}"
        );
        // The menu's items came back in the result: this task may use them.
        assert!(
            ask(
                &broker,
                prepare("t1", &steps(json!([{"type": "click", "ref": "r13"}])))
            )
            .await
            .ok
        );
    }

    #[tokio::test]
    async fn passwords_are_never_typed() {
        let (broker, _, _) = broker();
        looked(&broker, "t1").await;
        let typing = steps(json!([{"type": "type", "ref": "r20", "text": "hunter2"}]));
        let response = ask(&broker, prepare("t1", &typing)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::NOT_ALLOWED));
        assert!(response.message.unwrap().contains("password field"));
        // Clicking it is fine (the user may type in it after).
        assert!(
            ask(
                &broker,
                prepare("t1", &steps(json!([{"type": "click", "ref": "r20"}])))
            )
            .await
            .ok
        );
    }

    #[tokio::test]
    async fn the_focused_element_is_a_root_too() {
        let (broker, calls, _) = broker();
        ask(&broker, windows("t1")).await;
        let response = ask(&broker, observe("t1", "w1", Some("focused"))).await;
        assert!(response.ok, "{response:?}");
        let last = calls.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last["args"]["root"], "focused");
        assert!(last["args"].get("observe").is_none(), "{last}");
    }

    #[tokio::test]
    async fn another_task_voids_the_refs() {
        let (broker, calls, _) = broker();
        looked(&broker, "t1").await;
        let expect = bindings(&broker, "t1", &click_file()).await;
        // While t1 waits on the user, t2 reads the apps.
        ask(&broker, windows("t2")).await;
        let response = ask(&broker, act("t1", &click_file(), &expect)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::STALE), "{response:?}");
        assert!(acts(&calls).is_empty());
        // t1 looks again and gets a fresh approval's bindings.
        looked(&broker, "t1").await;
        let again = bindings(&broker, "t1", &click_file()).await;
        assert_eq!(again, expect);
        assert!(ask(&broker, act("t1", &click_file(), &again)).await.ok);
    }

    #[tokio::test]
    async fn uncertain_input_blocks_more_input_until_someone_looks() {
        let (broker, calls, script) = broker();
        looked(&broker, "t1").await;
        let expect = bindings(&broker, "t1", &click_file()).await;
        script.lock().unwrap().click = Some(
            "{\"ok\":false,\"code\":\"wait_timeout\",\"input_emitted\":true,\"completed\":1,\"requested\":2}"
                .into(),
        );
        let response = ask(&broker, act("t1", &click_file(), &expect)).await;
        assert!(response.ok, "HCU's own result: {response:?}");
        let response = ask(&broker, act("t1", &click_file(), &expect)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::INSPECT_FIRST));
        assert_eq!(acts(&calls).len(), 1);
        // Reading the window is the look it waits for.
        ask(&broker, observe("t1", "w1", None)).await;
        let expect = bindings(&broker, "t1", &click_file()).await;
        assert!(ask(&broker, act("t1", &click_file(), &expect)).await.ok);
        assert_eq!(acts(&calls).len(), 2);
    }

    #[tokio::test]
    async fn an_undelivered_result_counts_as_unseen() {
        let (broker, _, _) = broker();
        looked(&broker, "t1").await;
        let expect = bindings(&broker, "t1", &click_file()).await;
        // agentd went away (the task was stopped): no delivered().
        let (response, _undelivered) = broker.handle(act("t1", &click_file(), &expect)).await;
        assert!(response.ok);
        let response = ask(&broker, act("t1", &click_file(), &expect)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::INSPECT_FIRST));
    }

    #[tokio::test]
    async fn hcu_stopping_mid_batch_restarts_with_new_refs_and_waits_for_a_look() {
        let (broker, _, script) = broker();
        looked(&broker, "t1").await;
        let expect = bindings(&broker, "t1", &click_file()).await;
        script.lock().unwrap().hang = true;
        let response = ask(&broker, act("t1", &click_file(), &expect)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::UNAVAILABLE));
        assert!(response.message.unwrap().contains("may or may not have happened"));
        script.lock().unwrap().hang = false;
        // A new process: the old bindings are void, and input waits for a look.
        looked(&broker, "t1").await;
        let fresh = bindings(&broker, "t1", &click_file()).await;
        assert_ne!(fresh[0].epoch, expect[0].epoch);
        assert_eq!(
            ask(&broker, act("t1", &click_file(), &expect))
                .await
                .error
                .as_deref(),
            Some(refusal::CHANGED)
        );
        assert!(ask(&broker, act("t1", &click_file(), &fresh)).await.ok);
    }

    fn gui(value: Value) -> Vec<GuiAction> {
        serde_json::from_value(value).unwrap()
    }

    async fn screenshot(broker: &Broker<Fake>, task: &str) -> Response {
        ask(broker, Request::Screenshot { task: task.into() }).await
    }

    fn prepare_gui(task: &str, actions: &[GuiAction]) -> Request {
        Request::PrepareGui {
            task: task.into(),
            actions: actions.to_vec(),
        }
    }

    #[tokio::test]
    async fn the_screenshot_comes_back_as_an_image_and_a_frame() {
        let (broker, _, _) = broker();
        let response = screenshot(&broker, "t1").await;
        assert_eq!(response.image.unwrap().mime, "image/png");
        assert_eq!(response.frame, Some((1229, 768)));
    }

    #[tokio::test]
    async fn pixel_input_lands_only_on_the_apps_and_on_the_screenshot_it_was_planned_on() {
        let (broker, calls, script) = broker();
        // Without the task's own screenshot, nothing.
        let click = gui(json!([{"type": "click", "x": 100, "y": 100}]));
        assert_eq!(
            ask(&broker, prepare_gui("t1", &click)).await.error.as_deref(),
            Some(refusal::STALE)
        );
        screenshot(&broker, "t1").await;
        let binding = ask(&broker, prepare_gui("t1", &click)).await.gui.unwrap();
        assert_eq!(binding.size, (1229, 768));
        assert_eq!(
            (binding.targets[0].app.as_str(), binding.targets[0].pid),
            ("Mousepad", 41)
        );
        // (800, 100) in the screenshot is the terminal on screen; (10, 700) the desktop.
        for (x, y, on) in [(800, 100, "Terminal"), (10, 700, "Desktop")] {
            let response = ask(
                &broker,
                prepare_gui("t1", &gui(json!([{"type": "click", "x": x, "y": y}]))),
            )
            .await;
            assert_eq!(response.error.as_deref(), Some(refusal::NOT_ALLOWED));
            assert!(response.message.unwrap().contains(on));
        }
        // Text goes to the window clicked just before, even with the terminal focused.
        script.lock().unwrap().focused = 42;
        let typing = gui(json!([{"type": "click", "x": 100, "y": 100}, {"type": "type", "text": "hi"}]));
        assert!(ask(&broker, prepare_gui("t1", &typing)).await.ok);
        let alone = gui(json!([{"type": "type", "text": "hi"}]));
        assert_eq!(
            ask(&broker, prepare_gui("t1", &alone)).await.error.as_deref(),
            Some(refusal::NOT_ALLOWED)
        );
        // The window moved while the user decided: nothing is sent.
        let binding = ask(&broker, prepare_gui("t1", &click)).await.gui.unwrap();
        script.lock().unwrap().moved = true;
        let request = Request::Gui {
            task: "t1".into(),
            actions: click.clone(),
            expect: binding.clone(),
        };
        let response = ask(&broker, request).await;
        assert!(response.error.is_some(), "{response:?}");
        assert!(calls_of(&calls, "GUI", "click").is_empty());
        script.lock().unwrap().moved = false;
        // A read since (it takes a frame too) voids the screenshot.
        ask(&broker, windows("t1")).await;
        ask(&broker, observe("t1", "w1", None)).await;
        assert_eq!(
            ask(&broker, prepare_gui("t1", &click)).await.error.as_deref(),
            Some(refusal::STALE)
        );
        // A fresh one, and it goes; its result is the next frame.
        screenshot(&broker, "t1").await;
        let binding = ask(&broker, prepare_gui("t1", &click)).await.gui.unwrap();
        let request = Request::Gui {
            task: "t1".into(),
            actions: click.clone(),
            expect: binding,
        };
        let response = ask(&broker, request).await;
        assert!(response.ok && response.image.is_some(), "{response:?}");
        assert_eq!(
            calls_of(&calls, "GUI", "click")[0]["args"]["actions"][0],
            json!({"type": "click", "x": 100.0, "y": 100.0})
        );
        assert!(ask(&broker, prepare_gui("t1", &click)).await.ok);
        // Another task's screenshot isn't this one's.
        screenshot(&broker, "t2").await;
        assert_eq!(
            ask(&broker, prepare_gui("t1", &click)).await.error.as_deref(),
            Some(refusal::STALE)
        );
    }

    #[tokio::test]
    async fn a_zoom_looks_at_the_tasks_own_screenshot() {
        let (broker, _, _) = broker();
        let zoom = |task: &str| Request::Zoom {
            task: task.into(),
            region: [0.0, 0.0, 100.0, 100.0],
        };
        assert_eq!(
            ask(&broker, zoom("t1")).await.error.as_deref(),
            Some(refusal::STALE)
        );
        screenshot(&broker, "t1").await;
        assert!(ask(&broker, zoom("t1")).await.image.is_some());
    }

    #[test]
    fn a_star_stands_for_one_path_segment() {
        let slack = "/snap/slack/*/usr/lib/slack/slack";
        assert!(exe_matches(slack, "/snap/slack/271/usr/lib/slack/slack"));
        assert!(exe_matches(slack, "/snap/slack/272/usr/lib/slack/slack"));
        assert!(!exe_matches(slack, "/snap/slack/271/extra/usr/lib/slack/slack"));
        assert!(!exe_matches(slack, "/snap/other/271/usr/lib/slack/slack"));
        assert!(!exe_matches(slack, "/snap/slack/../usr/lib/slack/slack"));
        assert!(exe_matches("/usr/bin/mousepad", "/usr/bin/mousepad"));
        assert!(!exe_matches("/usr/bin/mousepad", "/usr/bin/mousepad2"));
    }

    #[test]
    fn matches_wmctrl_windows_by_title() {
        let listing = "0x02a00004  0 4242   neo-agent notes.txt - Mousepad\n0x02c00003  0 4343   neo-agent notes.txt - Mousepad (2)\n0x01e00007 -1 15661  neo-agent xfce4-panel\n";
        assert_eq!(wmctrl_owners(listing, "notes.txt - Mousepad"), vec![4242]);
        assert_eq!(wmctrl_owners(listing, "notes.txt - Mouse…"), vec![4242, 4343]);
        assert!(wmctrl_owners(listing, "Inbox").is_empty());
    }

    #[test]
    fn reads_the_stacking_order_and_window_geometry() {
        let stacking = "_NET_CLIENT_LIST_STACKING(WINDOW): window id # 0x1a0002c, 0x1800003, 0x2800003\n";
        assert_eq!(
            parse_stacking(stacking),
            vec!["0x1a0002c", "0x1800003", "0x2800003"]
        );
        let info = "xwininfo: Window id: 0x2800003 \"notes.txt - Mousepad\"\n\n  Absolute upper-left X:  10\n  Absolute upper-left Y:  52\n  Relative upper-left X:  0\n  Width: 640\n  Height: 480\n  Map State: IsViewable\n";
        assert_eq!(parse_xwininfo(info), Some((10, 52, 640, 480, true)));
        assert_eq!(
            parse_xwininfo(&info.replace("IsViewable", "IsUnMapped")),
            Some((10, 52, 640, 480, false))
        );
    }

    #[tokio::test]
    async fn only_the_client_user_gets_answers() {
        // Over a real socket: the peer check happens in main's accept loop,
        // so here the connection handler itself, with a request and its answer.
        let dir = std::env::temp_dir().join(format!("broker-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.sock");
        let listener = listener(Some(&path)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (broker, _, _) = broker();
        let broker = Arc::new(broker);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(broker, stream).await;
        });
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        stream.write_all(b"{\"op\":\"status\"}\n").await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        let reply: Response = serde_json::from_str(&reply).unwrap();
        assert!(reply.ok && reply.status.unwrap()["apps"][0] == "Mousepad");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
