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
use super::{
    Binding, Image, OffBus, Query, Request, Response, TextPage, WindowLine, evidence, find_element,
    input_uncertain, parse_apps, refs, refusal,
};

/// How long a request waits for the one before it.
const LOCK_WAIT: Duration = Duration::from_secs(60);
const READ_DEADLINE: Duration = Duration::from_secs(30);
/// A click, its settle wait (3 s) and the diff after it.
const CLICK_DEADLINE: Duration = Duration::from_secs(60);
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
            && !matches!(request, Request::Screenshot { .. })
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
            Request::Describe { window, element, .. } => {
                done(match self.describe(state, &window, &element).await? {
                    Ok(binding) => Response {
                        ok: true,
                        binding: Some(binding),
                        ..Response::default()
                    },
                    Err(refused) => refused,
                })
            }
            Request::Click {
                window,
                element,
                expect,
                ..
            } => self.click(state, &window, &element, &expect).await,
            Request::Screenshot { .. } => done(self.screenshot(state).await?),
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
                        && owners.iter().all(|(_, e)| *e == rule.exe)
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
        let exes: BTreeSet<&str> = self.rules.iter().map(|r| r.exe.as_str()).collect();
        for app in off {
            if state
                .backend
                .exe(app.pid)
                .is_some_and(|exe| exes.contains(exe.as_str()))
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

    /// The element as the app has it now: what an approval shows and binds.
    async fn describe(
        &self,
        state: &mut State<B>,
        window: &str,
        element: &str,
    ) -> Result<Result<Binding, Response>> {
        let w = match self.usable(state, window).await? {
            Ok(w) => w,
            Err(refused) => return Ok(Err(refused)),
        };
        if let Some(refused) = Self::shown(state, element) {
            return Ok(Err(refused));
        }
        // `full`: a repeated `tree` would come back as "Tree unchanged".
        let arguments = json!({"mode": "observe", "window_ref": window, "observe": "full"});
        let tree = self.call(state, "AX", arguments, READ_DEADLINE).await?;
        let found = (!tree.is_error)
            .then(|| find_element(&tree.text, element))
            .flatten();
        let Some(found) = found else {
            return Ok(Err(Response::refused(
                refusal::STALE,
                format!("{element} isn't in {window} now; observe it again"),
            )));
        };
        let rule = w.rule.expect("usable windows have a rule");
        Ok(Ok(Binding {
            epoch: state.epoch.clone().unwrap_or_default(),
            window: window.to_string(),
            element: element.to_string(),
            app: rule.name,
            app_id: w.line.app_id,
            pid: w.pid,
            exe: w.exe,
            title: w.line.title,
            role: found.role,
            name: found.name,
            path: found.path,
        }))
    }

    async fn click(
        &self,
        state: &mut State<B>,
        window: &str,
        element: &str,
        expect: &Binding,
    ) -> Result<(Response, Delivery)> {
        let refused = |code, message: String| Ok((Response::refused(code, message), Delivery(None)));
        if let Some(pending) = &state.pending {
            let which = if Some(&pending.epoch) == state.epoch.as_ref() {
                format!("window {}", pending.window)
            } else {
                "the app's window (list the windows: refs changed)".to_string()
            };
            return refused(
                refusal::INSPECT_FIRST,
                format!("an earlier input's outcome is unknown; observe {which} before any more input"),
            );
        }
        if state.epoch.as_deref() != Some(expect.epoch.as_str()) {
            return refused(
                refusal::CHANGED,
                "computer-use restarted since this was approved; its refs are gone".into(),
            );
        }
        let now = match self.describe(state, window, element).await? {
            Ok(binding) => binding,
            Err(refused) => return Ok((refused, Delivery(None))),
        };
        if &now != expect {
            return refused(
                refusal::CHANGED,
                format!("{element} isn't what was approved any more (the app changed)"),
            );
        }
        // Uncertain from here until the result reaches agentd.
        state.tokens += 1;
        let token = state.tokens;
        state.pending = Some(Pending {
            epoch: expect.epoch.clone(),
            window: window.to_string(),
            token,
        });
        let arguments = json!({
            "mode": "act",
            "window_ref": window,
            "actions": [
                {"type": "click", "ref": element, "allow_global_input": false},
                {"type": "wait", "settle": true, "timeout_ms": 3000},
            ],
            "observe": "diff",
        });
        let result = match self.call(state, "AX", arguments, CLICK_DEADLINE).await {
            Ok(result) => result,
            Err(err) => {
                return refused(
                    refusal::UNAVAILABLE,
                    format!(
                        "{err:#}. The click may or may not have happened: list the windows and observe before anything else"
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

    async fn screenshot(&self, state: &mut State<B>) -> Result<Response> {
        let result = self
            .call(
                state,
                "GUI",
                json!({"actions": [{"type": "screenshot"}]}),
                READ_DEADLINE,
            )
            .await?;
        let image = result
            .images
            .into_iter()
            .next()
            .map(|(mime, data)| Image { mime, data });
        Ok(Response {
            ok: image.is_some(),
            error: image.is_none().then(|| refusal::UNAVAILABLE.to_string()),
            message: image.is_none().then(|| "no screenshot came back".to_string()),
            image,
            ..Response::default()
        })
    }
}

fn op_name(request: &Request) -> &'static str {
    match request {
        Request::Status => "status",
        Request::Windows { .. } => "windows",
        Request::Observe { .. } => "observe",
        Request::Find { .. } => "find",
        Request::Describe { .. } => "describe",
        Request::Click { .. } => "click",
        Request::Screenshot { .. } => "screenshot",
    }
}

fn task_of(request: &Request) -> Option<&str> {
    match request {
        Request::Status => None,
        Request::Windows { task }
        | Request::Observe { task, .. }
        | Request::Find { task, .. }
        | Request::Describe { task, .. }
        | Request::Click { task, .. }
        | Request::Screenshot { task } => Some(task),
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

    fn exe(&mut self, pid: u32) -> Option<String> {
        (pid > 0)
            .then(|| std::fs::read_link(format!("/proc/{pid}/exe")).ok())
            .flatten()
            .map(|p| p.to_string_lossy().into_owned())
    }
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

    /// A desktop with one Mousepad window (w1), a terminal (w2) and Chrome (w3).
    struct Fake {
        session: Option<fake::Pipes>,
        calls: Arc<StdMutex<Vec<Value>>>,
        script: Arc<StdMutex<Script>>,
        starts: u32,
    }

    #[derive(Default)]
    struct Script {
        /// The File menu is open (its items are in the tree).
        menu_open: bool,
        /// What the next click returns (default: a clean success).
        click: Option<String>,
        /// The editor's own title.
        title: String,
        /// Clicks hang (the stream breaks mid-call).
        hang: bool,
    }

    const MOUSEPAD: &str = "/usr/bin/mousepad";

    fn tree(script: &Script) -> String {
        let items = if script.menu_open {
            "   - menuitem \"Save\" [ref=r13]:\n    - /description: Save the current document\n"
        } else {
            ""
        };
        format!(
            "{{\"ok\":true,\"input_emitted\":false}}\napp: \"org.xfce.mousepad\"\nwindow: \"{}\" [ref=w1]\nax:\n - window \"{}\":\n  - menu \"File\" [ref=r1]:\n   - /description: File menu\n{items}  - textbox [ref=r7]:\n",
            script.title, script.title
        )
    }

    impl Fake {
        fn new() -> (Self, Calls, Shared) {
            let calls = Arc::new(StdMutex::new(Vec::new()));
            let script = Arc::new(StdMutex::new(Script {
                title: "notes.txt - Mousepad".into(),
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
                            "{\"ok\":true,\"input_emitted\":true,\"completed\":2,\"requested\":2}\neffect: r1 menu \"File\" selected=false→true\n - menuitem \"Save\" [ref=r13]:".into()
                        });
                        fake::text(&text, false)
                    }
                    ("GUI", _) => json!({"content": [
                        {"type": "text", "text": "{\"ok\":true,\"input_emitted\":false}"},
                        {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="},
                    ], "isError": false}),
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
                50 => Some("/opt/google/chrome/chrome".into()),
                _ => None,
            }
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

    fn describe(task: &str, element: &str) -> Request {
        Request::Describe {
            task: task.into(),
            window: "w1".into(),
            element: element.into(),
        }
    }

    fn click(task: &str, element: &str, expect: &Binding) -> Request {
        Request::Click {
            task: task.into(),
            window: "w1".into(),
            element: element.into(),
            expect: expect.clone(),
        }
    }

    fn acts(calls: &Arc<StdMutex<Vec<Value>>>) -> Vec<Value> {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["args"]["mode"] == "act")
            .cloned()
            .collect()
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
    }

    #[tokio::test]
    async fn an_approved_click_goes_ahead_only_on_the_same_element() {
        let (broker, calls, script) = broker();
        ask(&broker, windows("t1")).await;
        ask(&broker, observe("t1", "w1", None)).await;
        let binding = ask(&broker, describe("t1", "r1")).await.binding.unwrap();
        assert_eq!(
            (binding.app.as_str(), binding.role.as_str(), binding.name.as_str()),
            ("Mousepad", "menu", "File")
        );
        assert_eq!((binding.pid, binding.exe.as_str()), (41, MOUSEPAD));
        assert_eq!(binding.path, "menu “File” — File menu");
        // The window's title changed while the user decided: refused.
        script.lock().unwrap().title = "other.txt - Mousepad".into();
        let response = ask(&broker, click("t1", "r1", &binding)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::CHANGED));
        assert!(acts(&calls).is_empty());
        // Whole-window reads ask for the full tree: HCU would elide a repeat.
        let observes: Vec<Value> = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["args"]["mode"] == "observe")
            .map(|c| c["args"].clone())
            .collect();
        assert!(
            observes.iter().all(|args| args["observe"] == "full"),
            "{observes:?}"
        );
        // As approved: one semantic click, then a settle wait.
        script.lock().unwrap().title = "notes.txt - Mousepad".into();
        let response = ask(&broker, click("t1", "r1", &binding)).await;
        assert!(response.ok && !response.hcu_error, "{response:?}");
        let act = &acts(&calls)[0]["args"];
        assert_eq!(act["window_ref"], "w1");
        assert_eq!(
            act["actions"][0],
            json!({"type": "click", "ref": "r1", "allow_global_input": false})
        );
        assert_eq!(act["actions"][1]["settle"], true);
        // The menu's items came back in the result: this task may use them.
        assert!(ask(&broker, describe("t1", "r13")).await.binding.is_some());
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
        ask(&broker, windows("t1")).await;
        ask(&broker, observe("t1", "w1", None)).await;
        let binding = ask(&broker, describe("t1", "r1")).await.binding.unwrap();
        // While t1 waits on the user, t2 reads the apps.
        ask(&broker, windows("t2")).await;
        let response = ask(&broker, click("t1", "r1", &binding)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::STALE), "{response:?}");
        assert!(acts(&calls).is_empty());
        // t1 looks again and gets a fresh approval's binding.
        ask(&broker, windows("t1")).await;
        ask(&broker, observe("t1", "w1", None)).await;
        let again = ask(&broker, describe("t1", "r1")).await.binding.unwrap();
        assert_eq!(again, binding);
        assert!(ask(&broker, click("t1", "r1", &again)).await.ok);
    }

    #[tokio::test]
    async fn uncertain_input_blocks_more_input_until_someone_looks() {
        let (broker, calls, script) = broker();
        ask(&broker, windows("t1")).await;
        ask(&broker, observe("t1", "w1", None)).await;
        let binding = ask(&broker, describe("t1", "r1")).await.binding.unwrap();
        script.lock().unwrap().click = Some(
            "{\"ok\":false,\"code\":\"wait_timeout\",\"input_emitted\":true,\"completed\":1,\"requested\":2}"
                .into(),
        );
        let response = ask(&broker, click("t1", "r1", &binding)).await;
        assert!(response.ok, "HCU's own result: {response:?}");
        let response = ask(&broker, click("t1", "r1", &binding)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::INSPECT_FIRST));
        assert_eq!(acts(&calls).len(), 1);
        // Reading the window is the look it waits for.
        ask(&broker, observe("t1", "w1", None)).await;
        let binding = ask(&broker, describe("t1", "r1")).await.binding.unwrap();
        assert!(ask(&broker, click("t1", "r1", &binding)).await.ok);
        assert_eq!(acts(&calls).len(), 2);
    }

    #[tokio::test]
    async fn an_undelivered_result_counts_as_unseen() {
        let (broker, _, _) = broker();
        ask(&broker, windows("t1")).await;
        ask(&broker, observe("t1", "w1", None)).await;
        let binding = ask(&broker, describe("t1", "r1")).await.binding.unwrap();
        // agentd went away (the task was stopped): no delivered().
        let (response, _undelivered) = broker.handle(click("t1", "r1", &binding)).await;
        assert!(response.ok);
        let response = ask(&broker, click("t1", "r1", &binding)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::INSPECT_FIRST));
    }

    #[tokio::test]
    async fn hcu_stopping_mid_click_restarts_with_new_refs_and_waits_for_a_look() {
        let (broker, _, script) = broker();
        ask(&broker, windows("t1")).await;
        ask(&broker, observe("t1", "w1", None)).await;
        let binding = ask(&broker, describe("t1", "r1")).await.binding.unwrap();
        script.lock().unwrap().hang = true;
        let response = ask(&broker, click("t1", "r1", &binding)).await;
        assert_eq!(response.error.as_deref(), Some(refusal::UNAVAILABLE));
        assert!(response.message.unwrap().contains("may or may not have happened"));
        script.lock().unwrap().hang = false;
        // A new process: the old binding is void, and input waits for a look.
        ask(&broker, windows("t1")).await;
        ask(&broker, observe("t1", "w1", None)).await;
        let fresh = ask(&broker, describe("t1", "r1")).await.binding.unwrap();
        assert_ne!(fresh.epoch, binding.epoch);
        assert_eq!(
            ask(&broker, click("t1", "r1", &binding)).await.error.as_deref(),
            Some(refusal::CHANGED)
        );
        assert!(ask(&broker, click("t1", "r1", &fresh)).await.ok);
    }

    #[tokio::test]
    async fn the_screenshot_comes_back_as_an_image() {
        let (broker, _, _) = broker();
        let response = ask(&broker, Request::Screenshot { task: "t1".into() }).await;
        assert_eq!(response.image.unwrap().mime, "image/png");
    }

    #[test]
    fn matches_wmctrl_windows_by_title() {
        let listing = "0x02a00004  0 4242   neo-agent notes.txt - Mousepad\n0x02c00003  0 4343   neo-agent notes.txt - Mousepad (2)\n0x01e00007 -1 15661  neo-agent xfce4-panel\n";
        assert_eq!(wmctrl_owners(listing, "notes.txt - Mousepad"), vec![4242]);
        assert_eq!(wmctrl_owners(listing, "notes.txt - Mouse…"), vec![4242, 4343]);
        assert!(wmctrl_owners(listing, "Inbox").is_empty());
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
