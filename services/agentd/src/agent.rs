//! The agent: runs each conversation's tasks one at a time, in a lane of its
//! own (up to `MAX_PARALLEL_TASKS` conversations at once; more messages wait
//! in a queue, sessions.rs), emits events for the apps, and asks the user for
//! approvals and "action needed". Scheduled runs go beside the conversations,
//! one at a time, from a queue (schedule.rs), so a chat is never kept waiting
//! for one: they work in a browser window of their own, and their report
//! reaches their conversation when they end.
//!
//! A task that needs an approval parks: its state is saved, its lane is free
//! again, and the user's answer resumes it from there (after a restart too).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast, watch};

use crate::access;
use crate::browser::Browser;
use crate::cdp::Cdp;
use crate::clock::UserClock;
use crate::config::Config;
use crate::guard;
use crate::llm::{self, Together, Usage, truncate};
use crate::memory;
use crate::plan;
use crate::policy::Policy;
use crate::schedule;
use crate::secrets::{self, Secrets};
use crate::shell::{KeyFile, Route, Shell, split_sudo};
use crate::skills::Skills;
use crate::store::{Event, MAIN, Pending, QueuedRun, Schedule, SecretInfo, Store};
use crate::supervise::{self, Verdict};
use crate::tools;

const HISTORY_LIMIT: i64 = 20;
/// Memories shown to the model at the start of each task (the latest ones).
const MEMORY_LIMIT: i64 = 60;
const SCHEDULE_PROMPT_MAX_CHARS: usize = 2000;
const SCHEDULE_NAME_MAX_CHARS: usize = 60;
const SECRET_LABEL_MAX_CHARS: usize = 60;
/// An approval answered later than this doesn't resume its task: the context
/// is stale.
const APPROVAL_TTL_SECS: i64 = 24 * 3600;
const PAGE_CHANGED: &str = "the page changed after the user approved, so nothing was done; look at the page \
     again and ask again if it's still needed";
const POLICY_CHANGED: &str =
    "The rules changed while this was waiting, so I didn't do it. Ask again if you still want it.";
/// Added to a scheduled run's framing with memory v2 (docs/memory-v2.md §15).
const REPORT_OUTCOME_RULE: &str = "End with report_outcome. Use outcome=nothing when there is nothing the user \
     needs to see, outcome=result with a summary under 180 characters when there is, and outcome=blocked when you \
     could not finish. The user is not watching; a result they can use is worth their attention, nothing new is not.";
/// Added to a scheduled run's framing: it never waits on a card.
const UNATTENDED_RULE: &str = "A scheduled run cannot wait for approval. Browser inputs, saved-key use,      direct internet, system administration and actions requiring review are unavailable. Report what was blocked.";
/// Added to runs allowed to bypass ordinary shell review.
const AUTO_APPROVE_RULE: &str = "Ordinary worker commands can run without asking. Browser inputs, saved-key use,      direct internet, system administration and data transfers requiring review are still unavailable. Report what was blocked.";
/// Maximum size for browser typing. The card always shows the whole value.
const CARD_TEXT_MAX_CHARS: usize = 4000;
const INTERRUPTED: &str =
    "I was interrupted: the agent on your desktop restarted. Ask me to continue if you still need this.";
/// Memory v2: the one reminder when the user asked to remember something and
/// the task is ending without a save. Worded so a question ("do you
/// remember…?") just gets its answer again.
/// The one reminder after a reply with neither text nor tool calls: usually the
/// model's thinking used up `max_tokens` (finish_reason "length").
const EMPTY_REPLY: &str = "(agentd) Your last reply was cut off while you were still thinking, before you \
     wrote anything. Think less this time: make your next tool call, or if the task is done, give your answer.";
/// Empty replies in a row that are asked again, each with more room
/// (`turn_max_tokens`), before the task ends with a summary instead.
const EMPTY_RETRIES: u8 = 2;
/// A model turn's output limit; doubled for each retry of an empty reply.
const TURN_MAX_TOKENS: u32 = 8192;
/// Supervisor checks in a row that can't give a verdict before the task is
/// treated as stuck: a broken check doesn't leave a task running unchecked.
const REVIEW_FAILURES: u8 = 2;
const REMEMBER_NUDGE: &str = "(agentd) The user's message asked you to remember something, and you haven't \
     called remember. If they did, call remember with it now, then finish. If they only asked what you remember, \
     give your answer again.";
/// "remember", "don't forget", "keep in mind", "from now on" (memory v2 §7).
static ASKS_TO_REMEMBER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(remember|don'?t forget|keep in mind|from now on)\b").expect("valid pattern")
});

/// `prefix` (ASCII) at the start of `text`, in any letter case: what follows it.
fn strip_prefix_ignore_case<'s>(text: &'s str, prefix: &str) -> Option<&'s str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

/// A memory change the signed-in user typed into the chat, applied without
/// the model: `remember rule: …`, `remember preference: …`, `remember fact: …`
/// or `forget #ID`, in any letter case (phone keyboards capitalize the first
/// word), at the start of the message.
#[derive(Debug, PartialEq)]
enum MemoryCommand<'a> {
    Remember {
        kind: &'static str,
        text: &'a str,
    },
    /// The id's digits as typed: one too large for an id is still a command,
    /// answered with "No memory #…".
    Forget(&'a str),
}

impl<'a> MemoryCommand<'a> {
    /// Only for a chat message from the API's Unix socket with a signed-in
    /// caller (tailnet-proxy's header); never a scheduled run's prompt.
    fn from_authenticated_chat(request: &'a str, listener: &str, caller: Option<&str>) -> Option<Self> {
        if listener.starts_with("unix:") && caller.is_some_and(|login| !login.trim().is_empty()) {
            Self::parse(request)
        } else {
            None
        }
    }

    fn parse(request: &'a str) -> Option<Self> {
        let request = request.trim();
        for (prefix, kind) in [
            ("remember rule:", "rule"),
            ("remember preference:", "preference"),
            ("remember fact:", "fact"),
        ] {
            if let Some(text) = strip_prefix_ignore_case(request, prefix) {
                return Some(Self::Remember {
                    kind,
                    text: text.trim(),
                });
            }
        }
        let rest = strip_prefix_ignore_case(request, "forget #")?;
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let (id, after) = rest.split_at(digits);
        // "forget #12." is the command; "forget #12 and #13" is for the model.
        let trailing_ok = after
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, '.' | ',' | '!' | '?' | ';' | ':' | '…'));
        (!id.is_empty() && trailing_ok).then_some(Self::Forget(id))
    }

    fn summary(&self) -> String {
        match self {
            Self::Remember { kind, .. } => format!("Remember a {kind}"),
            Self::Forget(id) => format!("Forget memory #{id}"),
        }
    }
}

fn is_memory_tool(name: &str) -> bool {
    matches!(name, "remember" | "forget" | "search_memory")
}

/// A parked task's checkpoint keeps no memory text: memory lives in the memory
/// table only, so forgetting, editing or purging a memory leaves no copy behind
/// in `pending` (the same reason the v8 migration strips memory steps' args),
/// and a resumed task gets its notes fresh (`run_loop`) rather than a stale or
/// forgotten note. That covers the system prompt (its standing notes), memory
/// calls that already ran (their arguments) and their results (search hits).
/// Calls still queued haven't run: their arguments are the model's request,
/// not stored memory, and they run as written when the task resumes.
fn scrub_memory_checkpoint(checkpoint: &mut Value) {
    let last_was_memory = checkpoint["loops"]["last"]
        .as_str()
        .and_then(|call| call.split_once(' '))
        .is_some_and(|(tool, _)| is_memory_tool(tool));
    if last_was_memory {
        checkpoint["loops"] = json!(guard::LoopDetector::default());
    }
    let queued: HashSet<String> = checkpoint["queue"]
        .as_array()
        .map(|queue| {
            queue
                .iter()
                .filter_map(|call| call["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut calls = HashSet::new();
    let mut scrub_call = |call: &mut Value| {
        let pending = call["id"].as_str().is_some_and(|id| queued.contains(id));
        if !pending && call["function"]["name"].as_str().is_some_and(is_memory_tool) {
            call["function"]["arguments"] = json!("{}");
            if let Some(id) = call["id"].as_str() {
                calls.insert(id.to_string());
            }
        }
    };
    if let Some(messages) = checkpoint["messages"].as_array_mut() {
        if let Some(system) = messages.first_mut() {
            system["content"] = json!("");
        }
        for message in &mut *messages {
            if let Some(tool_calls) = message["tool_calls"].as_array_mut() {
                tool_calls.iter_mut().for_each(&mut scrub_call);
            }
        }
        for message in messages {
            if message["tool_call_id"]
                .as_str()
                .is_some_and(|id| calls.contains(id))
            {
                message["content"] = json!("Memory context is refreshed after a pause.");
            }
        }
    }
}

fn load_parked_state(checkpoint: &str) -> Option<RunState> {
    let mut value = serde_json::from_str(checkpoint).ok()?;
    scrub_memory_checkpoint(&mut value);
    serde_json::from_value(value).ok()
}

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    pub model: String,
    pub allow_actions: bool,
    /// A person can enable automatic review for eligible operations.
    pub auto_approve: bool,
    pub auto_approve_version: String,
    /// Memory v2's consolidation: off, observe or auto (consolidate.rs).
    pub memory_consolidation: String,
    pub memory_consolidation_model: String,
    pub memory_consolidation_daily_cap_usd: f64,
    /// What the model may cost a month, in USD (usage.rs); None: no budget.
    pub monthly_budget_usd: Option<f64>,
}

pub enum StartError {
    Busy,
    NotFound,
}

/// What became of a message: a task started, or it waits in its
/// conversation's queue (`position` 1 is next).
#[derive(Debug, PartialEq)]
pub enum Posted {
    Started(String),
    Queued { id: String, position: i64 },
}

impl Posted {
    /// The API's answer: `{task}` or `{queued: {id, position}}`.
    pub fn json(&self) -> Value {
        match self {
            Self::Started(task) => json!({"task": task}),
            Self::Queued { id, position } => json!({"queued": {"id": id, "position": position}}),
        }
    }
}

/// Why a key from the app wasn't saved.
pub enum SecretError {
    /// Bad name or value, or too many keys: tell the user.
    Invalid(String),
    Failed(anyhow::Error),
}

/// A task's state between model turns: all it needs to carry on after parking
/// on an approval, across restarts too.
#[derive(Serialize, Deserialize)]
struct RunState {
    task: String,
    model: String,
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
    /// The schedule's name, when the task is a scheduled run.
    #[serde(default)]
    scheduled: Option<String>,
    /// The schedule's id, when the task is a scheduled run.
    #[serde(default)]
    schedule_id: Option<i64>,
    /// Memory v2: the user's message asked to remember something…
    #[serde(default)]
    remember_asked: bool,
    /// …and remember was called…
    #[serde(default)]
    remembered: bool,
    /// …or the model was reminded once.
    #[serde(default)]
    nudged: bool,
    /// Search terms carried over when the user's new request is a resume marker.
    #[serde(default)]
    memory_query: Option<String>,
    /// A scheduled run's `report_outcome` (result, nothing or blocked).
    #[serde(default)]
    reported: Option<schedule::Reported>,
    /// Empty replies in a row so far (asked again, with more room each time).
    #[serde(default)]
    empty_retries: u8,
    /// The agent's plan (update_plan): the user's checklist, the supervisor's
    /// measure of progress.
    #[serde(default)]
    plan: Vec<plan::Item>,
    /// When the supervisor last checked (supervise.rs).
    #[serde(default)]
    review: ReviewClock,
    /// What the user asked, or the schedule's prompt: a scheduled run's
    /// report shows it as its instructions.
    #[serde(default)]
    request: String,
    /// The conversation it belongs to (`main` for tasks from before them).
    #[serde(default = "main_session")]
    session: String,
}

fn main_session() -> String {
    MAIN.to_string()
}

/// When the supervisor last checked on a task, and what it said.
#[derive(Clone, Default, Serialize, Deserialize)]
struct ReviewClock {
    /// The step, working seconds and conversation length at the last check.
    at_step: u32,
    at_secs: f64,
    at_message: usize,
    /// Checks in a row that gave no verdict.
    failures: u8,
    /// The last check's verdict and advice, when it wasn't "progressing".
    previous: Option<String>,
    /// A stuck card's reason and advice, for when the user answers it.
    stuck: Option<(String, String)>,
}

impl ReviewClock {
    fn checked(&mut self, step: u32, secs: f64, messages: usize) {
        self.at_step = step;
        self.at_secs = secs;
        self.at_message = messages;
        self.failures = 0;
    }
}

/// A model turn's output limit: more room for each retry of an empty reply,
/// which was most likely cut off while the model was thinking.
fn turn_max_tokens(empty_retries: u8) -> u32 {
    TURN_MAX_TOKENS << empty_retries.min(EMPTY_RETRIES)
}

/// The scheduled run the background worker is on.
struct Background {
    task: String,
    schedule: i64,
    /// 0, or 1 for the retry.
    attempt: i64,
}

/// A conversation's history while its task is between its question and its
/// answer: scheduled runs' reports that end meanwhile wait, so the question
/// and its answer stay together for the model.
#[derive(Default)]
struct Thread {
    open: bool,
    held: Vec<(&'static str, String)>,
}

/// A conversation's chat lane: the task running in it (not one parked on a
/// card), the watch that stops it, and its history thread.
pub(crate) struct Lane {
    pub(crate) running: Option<String>,
    pub(crate) cancel: watch::Sender<bool>,
    thread: Thread,
}

impl Lane {
    pub(crate) fn new() -> Self {
        Self {
            running: None,
            cancel: watch::channel(false).0,
            thread: Thread::default(),
        }
    }
}

/// Event kinds about the whole desktop, never one conversation: they carry
/// no `session` even when they name a task.
fn is_global(kind: &str) -> bool {
    matches!(kind, "settings" | "auto_approve.changed" | "budget.alert")
        || ["secret.", "access.", "session."]
            .iter()
            .any(|prefix| kind.starts_with(prefix))
}

/// Added to a command's output when the proxy refused a host.
const ACCESS_HINT: &str = "\n\n(agentd: the desktop's proxy refused a site that isn't on its list. If the task \
     needs it, call request_access with that exact host and why; don't try to get around the proxy.)";
const ACCESS_HINT_SCHEDULED: &str = "\n\n(agentd: the desktop's proxy refused a site that isn't on its list. A \
     scheduled run can't ask for it: say in your report which host it needs.)";
/// Added to a failed command's output when it seems to have connected on its
/// own, which the guard firewall refuses.
const DIRECT_HINT: &str = "\n\n(agentd: this command seems to have connected to the internet on its own, not \
     through the desktop's proxy, and the firewall refuses that; there's no DNS outside the proxy either. \
     request_access can't help, since it only adds sites to the proxy's list. Look for the tool's proxy setting or \
     a mode that uses HTTPS_PROXY (a skill may name it). If it has none, run this one command again with \
     direct_internet true, which the user approves on a card, or tell the user what has to run elsewhere. Don't \
     try to get around the firewall any other way.)";
const DIRECT_HINT_SCHEDULED: &str = "\n\n(agentd: this command seems to have connected to the internet on its \
     own, not through the desktop's proxy, and the firewall refuses that. A scheduled run can't ask for direct \
     access: say in your report what it needs.)";

/// Added to a failed command's output when it couldn't reach a server on this
/// machine: the guard firewall lets commands reach only the proxy (a direct
/// command, only public addresses), even a server they start themselves.
const LOCAL_HINT: &str = "\n\n(agentd: this command tried to connect to a server on this machine (localhost), \
     and the desktop's firewall refuses that for your commands, including a server the same command started. \
     It isn't the server being down, and request_access or direct_internet can't change it. Test without a local \
     server (for example, the tests that don't need one), or tell the user what has to run elsewhere.)";
const LOCAL_HINT_SCHEDULED: &str = "\n\n(agentd: this command tried to connect to a server on this machine \
     (localhost), and the desktop's firewall refuses that, including a server the same command started. Say in \
     your report what has to run elsewhere.)";

/// Why a direct command asks: a shell card's "It …".
const DIRECT_REASON: &str =
    "connects directly to the internet and can reach any site, not just the allowed ones";

/// The proxy's port when `SHELL_PROXY` doesn't say (Ansible's default).
const DEFAULT_PROXY_PORT: u16 = 3128;

/// What a command's output says about its way out, for the model (access.rs).
/// `route`: how the guard firewall let it out; None without one (local
/// development). `proxy_port`: a refused connection there is the proxy's, not
/// a local server's.
fn egress_hint(
    text: &str,
    code: Option<i32>,
    route: Option<Route>,
    unattended: bool,
    proxy_port: u16,
) -> Option<&'static str> {
    if access::refused_by_proxy(text) {
        Some(if unattended {
            ACCESS_HINT_SCHEDULED
        } else {
            ACCESS_HINT
        })
    } else if route.is_some() && code != Some(0) && access::connected_locally(text, proxy_port) {
        Some(if unattended {
            LOCAL_HINT_SCHEDULED
        } else {
            LOCAL_HINT
        })
    } else if route == Some(Route::Proxy) && code != Some(0) && access::connected_directly(text) {
        Some(if unattended {
            DIRECT_HINT_SCHEDULED
        } else {
            DIRECT_HINT
        })
    } else {
        None
    }
}

/// A scheduled run that failed is tried once more after this long.
const RETRY_AFTER_SECS: i64 = 120;

/// The last second of a local date (`YYYY-MM-DD`) in `timezone` (UTC if
/// unknown), as a unix time. Touches no store, so it's safe anywhere.
pub fn end_of_day_in(date: &str, timezone: &str) -> Option<i64> {
    let day: jiff::civil::Date = date.parse().ok()?;
    let zone = jiff::tz::TimeZone::get(timezone).unwrap_or(jiff::tz::TimeZone::UTC);
    let next = day.tomorrow().ok()?.to_zoned(zone).ok()?;
    Some(next.timestamp().as_second() - 1)
}

/// A reply with no text to show (null, empty or only whitespace).
fn is_empty_reply(message: &Value) -> bool {
    message["content"]
        .as_str()
        .is_none_or(|text| text.trim().is_empty())
}

/// The user's answer to the approval a resumed task parked on.
struct Answer {
    approved: bool,
    /// The card they answered, to check the action is still the same one.
    card: Value,
    /// Who answered (the app's signed-in person), when known.
    by: Option<String>,
    /// What they changed the card's text to, if they did.
    edited: Option<String>,
}

/// What an approval decided.
enum Decision {
    Approved,
    Denied,
    /// Approved, but the action as it would run now isn't the one on the card.
    Changed,
    /// A scheduled run: nobody is there to answer, so it isn't done.
    Unattended,
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
    pub(crate) cdp: Cdp,
    policy: Arc<Policy>,
    shell: Shell,
    skills: Skills,
    /// Each conversation's chat lane: its running task (not one parked on an
    /// approval), its Stop and its history thread (sessions.rs).
    pub(crate) lanes: Mutex<HashMap<String, Lane>>,
    /// The conversation each task in progress belongs to, for its events.
    task_sessions: Mutex<HashMap<String, String>>,
    /// The scheduled run in progress, beside the conversations (schedule.rs).
    background: Mutex<Option<Background>>,
    /// Stops the scheduled run in progress.
    background_cancel: watch::Sender<bool>,
    /// Wakes the background worker when a run is queued.
    pub runs_queued: Notify,
    /// Shell commands: ordinary ones side by side (each its own unit and
    /// pipes), a direct one alone, since it opens the guard firewall to the
    /// shell user (§6.21).
    shell_turn: tokio::sync::RwLock<()>,
    access_sync: Mutex<()>,
    /// Budget alerts are checked and recorded one at a time (usage.rs).
    pub(crate) budget_check: Mutex<()>,
    /// Wakes the scheduler when schedules are added or resumed.
    pub schedules_changed: Notify,
    /// Wakes the effects runner when an effect is enqueued (effects.rs).
    pub effects_changed: Notify,
    /// Keys and tokens the user saved; their values are redacted from
    /// everything the model and the apps see.
    pub secrets: Secrets,
    /// The event seq of the tool call each task is running (a memory's source).
    step_seqs: Mutex<HashMap<String, i64>>,
    /// APNs for the iPhone and Mac apps' notifications (apns.rs), when an APNs
    /// key is configured; None leaves the apps' notifications off.
    pub apns: Option<crate::apns::Apns>,
}

pub(crate) fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Whether an action, looked at again when its task resumes, is the one the
/// user approved: same kind, element and what's around it, text, key, site or
/// command. What's around it matters: found by name, "Delete" could be another
/// row's by then.
/// The model's plain words for a command, as the card shows them: one line,
/// at most 120 characters; none when empty.
fn clip_summary(summary: &str) -> Option<String> {
    let line = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(120) {
        _ if line.is_empty() => None,
        Some((cut, _)) => Some(format!("{}…", line[..cut].trim_end())),
        None => Some(line),
    }
}

/// Which tool argument and which card field hold the text of a card the user
/// can edit: a command, what to type, or a schedule's instructions. A card says
/// it's editable, so an app never offers an edit to an agentd that would ignore
/// it and run the agent's words instead.
fn editable_text(card: &Value) -> Option<(&'static str, &'static str)> {
    if card["editable"] != true {
        return None;
    }
    match card["kind"].as_str()? {
        "shell" => Some(("command", "command")),
        "browser" if card["action"] == "type" => Some(("text", "text")),
        "schedule" => Some(("prompt", "text")),
        _ => None,
    }
}

/// The saved keys a call to run_shell asks for by name.
fn asked_secrets(args: &Value) -> Vec<String> {
    match &args["secrets"] {
        Value::Array(names) => names.iter().filter_map(Value::as_str).map(String::from).collect(),
        Value::String(name) => vec![name.clone()],
        _ => Vec::new(),
    }
}

fn same_request(approved: &Value, now: &Value) -> bool {
    [
        "kind",
        "action",
        "role",
        "target",
        "around",
        "text",
        "key",
        "submit",
        "site",
        "command",
        "direct",
        "when",
        "name",
        "secrets",
        "hosts",
        "binding",
        "url",
        "destination",
        "timeout",
        "manual",
    ]
    .iter()
    .all(|key| approved[key] == now[key])
}

impl Agent {
    pub fn new(cfg: Config) -> Result<Self> {
        let store = Store::open(&cfg.state_dir.join("agentd.db")).context("opening the state database")?;
        let policy = Arc::new(Policy::load(&cfg.policy_path)?);
        let (bus, _) = broadcast::channel(1024);
        let secrets = Secrets::open(&cfg.state_dir.join("secrets")).context("opening the saved keys")?;
        let agent = Self {
            secrets,
            llm: Together::new(&cfg)?,
            cdp: Cdp::new(&cfg.cdp_url)?,
            shell: Shell {
                run_as: cfg.run_as.clone(),
                workdir: cfg.workdir.clone(),
            },
            policy,
            skills: Skills::load(&cfg.skills_dir),
            store: Arc::new(store),
            bus,
            lanes: Mutex::default(),
            task_sessions: Mutex::default(),
            background: Mutex::new(None),
            background_cancel: watch::channel(false).0,
            runs_queued: Notify::new(),
            shell_turn: tokio::sync::RwLock::new(()),
            access_sync: Mutex::new(()),
            budget_check: Mutex::new(()),
            schedules_changed: Notify::new(),
            effects_changed: Notify::new(),
            step_seqs: Mutex::default(),
            apns: crate::apns::Apns::from_config(&cfg).context("the APNs key (APNS_AUTH_KEY)")?,
            cfg,
        };
        agent.recover();
        // Squid's list of the sites the user allowed, as the database has it.
        if let Err(err) = agent.sync_access() {
            tracing::error!(err = %format!("{err:#}"), "couldn't write the sites the user allowed for Squid");
        }
        Ok(agent)
    }

    /// After a restart: tasks that were running were cut off, so say so. Parked
    /// ones keep waiting for their approval.
    fn recover(&self) {
        // A first attempt may retry only if no action was dispatched.
        let now = unix_now();
        for run in self.store.running_scheduled_runs().unwrap_or_default() {
            let may_replay = self.store.run_has_no_actions(&run.id).unwrap_or(false);
            let recorded = self.store.set_run_status(&run.id, "interrupted").is_ok();
            let retried = recorded
                && may_replay
                && run.attempt == 0
                && now - run.due <= schedule::MAX_LATE_SECS
                && self.retry(run.schedule, now);
            if !may_replay {
                let schedule = self.store.schedule(run.schedule).ok().flatten();
                let name = schedule
                    .as_ref()
                    .map_or_else(|| format!("#{}", run.schedule), |s| s.name.clone());
                let session = schedule.map_or_else(main_session, |s| s.session);
                self.say_in(
                    &session,
                    &format!(
                        "Scheduled task “{name}” was interrupted. Check for changes before running it again."
                    ),
                );
            }
            tracing::warn!(task = %run.id, schedule = run.schedule, retried, "scheduled run interrupted by a restart");
        }
        // Each conversation's task that was running gets its answer in its
        // own history; its queued messages start once the socket is bound.
        for task in self.store.runs_with_status("running").unwrap_or_default() {
            tracing::warn!(task = %task, "task interrupted by a restart");
            let session = self.session_of(&task).unwrap_or_else(main_session);
            let _ = self.store.add_history_in(&session, "assistant", INTERRUPTED);
            self.emit(
                "message",
                json!({"role": "assistant", "text": INTERRUPTED, "task": task, "session": session}),
            );
            self.emit(
                "task.finished",
                json!({"task": task, "status": "interrupted", "session": session}),
            );
            let _ = self.store.set_run_status(&task, "interrupted");
        }
        // Nothing waits for the user's approval since 0.15.2.
        match memory::settle_pending(&self.store) {
            Ok(0) => {}
            Ok(settled) => tracing::info!(settled, "settled memories left waiting for approval"),
            Err(err) => tracing::error!(%err, "couldn't settle memories left waiting for approval"),
        }
    }

    /// Logs an event and sends it to the apps; returns its seq. An event
    /// about a task names its conversation (`session`), so the apps can
    /// route it; global kinds never do.
    pub fn emit(&self, kind: &str, mut data: Value) -> Option<i64> {
        // Every event names its agent: "main" until there are bots.
        if let Some(object) = data.as_object_mut() {
            object.entry("agent").or_insert_with(|| json!("main"));
            if is_global(kind) {
                object.remove("session");
            } else if !object.contains_key("session")
                && let Some(session) = object
                    .get("task")
                    .and_then(Value::as_str)
                    .and_then(|task| self.session_of(task))
            {
                object.insert("session".into(), json!(session));
            }
        }
        // No saved key's value reaches the log or the apps.
        self.secrets.redact_json(&mut data);
        match self.store.append_event(kind, &data) {
            Ok(event) => {
                let seq = event.seq;
                let _ = self.bus.send(event);
                Some(seq)
            }
            Err(err) => {
                tracing::error!(%err, kind, "couldn't store an event");
                None
            }
        }
    }

    /// Tells the apps about a memory v2 save: `memory.saved` (maybe
    /// replacing one: `memory.superseded`).
    pub fn announce_saved(&self, saved: &memory::Saved, by: &str, task: Option<&str>) {
        let memory::Saved::Active { item, superseded } = saved else {
            return;
        };
        self.emit(
            "memory.saved",
            json!({"task": task, "id": item.id, "trusted": item.trusted,
                   "kind": item.kind, "trust": item.trust, "status": item.status, "by": by}),
        );
        if let Some(old) = superseded {
            self.emit(
                "memory.superseded",
                json!({"task": task, "id": old, "by_id": item.id, "by": by}),
            );
        }
        // Metadata only: never memory text in the journal.
        tracing::info!(memory = item.id, kind = %item.kind, trust = %item.trust, status = %item.status, "memory saved");
    }

    pub fn settings(&self) -> Settings {
        let get = |key: &str| self.store.setting(key).ok().flatten();
        let auto_approve = get("auto_approve").unwrap_or_else(|| "initial".into());
        Settings {
            model: get("model").unwrap_or_else(|| self.cfg.model.clone()),
            // On by default: writing and sending in the browser ask first.
            allow_actions: get("allow_actions").as_deref() != Some("false"),
            // Off by default.
            auto_approve: auto_approve == "true" || auto_approve.starts_with("true:"),
            auto_approve_version: auto_approve,
            memory_consolidation: crate::consolidate::mode(&self.store).name().to_string(),
            memory_consolidation_model: crate::consolidate::model(&self.store, &self.cfg.consolidation_model),
            memory_consolidation_daily_cap_usd: crate::consolidate::daily_cap(&self.store),
            monthly_budget_usd: crate::usage::monthly_budget(&self.store),
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

    /// An enable must match the revision the user reviewed. A disable always
    /// rotates the revision, including when Auto approve was already off.
    pub fn set_auto_approve(
        &self,
        enabled: bool,
        expected_version: Option<&str>,
        by: Option<&str>,
    ) -> Result<Option<Settings>> {
        let value = format!("{enabled}:{}", uuid::Uuid::new_v4().simple());
        if enabled {
            let current = self.store.setting("auto_approve")?;
            if expected_version != Some(current.as_deref().unwrap_or("initial"))
                || !self
                    .store
                    .compare_and_set_setting("auto_approve", current.as_deref(), &value)?
            {
                return Ok(None);
            }
        } else {
            self.store.set_setting("auto_approve", &value)?;
        }
        tracing::warn!(enabled, "auto approve changed");
        self.emit("auto_approve.changed", json!({"on": enabled, "by": by}));
        let settings = self.settings();
        self.emit("settings", json!(settings));
        Ok(Some(settings))
    }

    /// Memory v2's consolidation settings; Err says what's wrong with a value.
    pub fn update_memory_settings(
        &self,
        mode: Option<&str>,
        model: Option<&str>,
        daily_cap_usd: Option<f64>,
    ) -> Result<Result<Settings, String>> {
        if let Some(mode) = mode {
            // An older app's "observe" is saved as what it means now: auto.
            let Some(mode) = crate::consolidate::Mode::parse(mode) else {
                return Ok(Err("memory_consolidation must be off or auto".into()));
            };
            self.store.set_setting("memory_consolidation", mode.name())?;
        }
        if let Some(model) = model {
            if !llm::KNOWN_MODELS.contains(&model) {
                return Ok(Err(format!(
                    "memory_consolidation_model must be one of {}",
                    llm::KNOWN_MODELS.join(", ")
                )));
            }
            self.store.set_setting("memory_consolidation_model", model)?;
        }
        if let Some(cap) = daily_cap_usd {
            if !(0.0..=10.0).contains(&cap) {
                return Ok(Err(
                    "memory_consolidation_daily_cap_usd must be between 0 and 10".into()
                ));
            }
            self.store
                .set_setting("memory_consolidation_daily_cap_usd", &cap.to_string())?;
        }
        let settings = self.settings();
        self.emit("settings", json!(settings));
        Ok(Ok(settings))
    }

    /// One model call outside a task (consolidation), through the same
    /// client as the tasks'.
    pub async fn complete(&self, request: &Value) -> Result<Value> {
        self.llm.chat(request).await
    }

    /// `main` is working: what an older app shows as the chat working.
    pub fn is_busy(&self) -> bool {
        self.is_working_in(MAIN)
    }

    /// A task of this conversation is running (not parked on a card).
    pub fn is_working_in(&self, session: &str) -> bool {
        self.lanes
            .lock()
            .unwrap()
            .get(session)
            .is_some_and(|lane| lane.running.is_some())
    }

    /// The conversations working now: (conversation, task).
    pub fn working(&self) -> Vec<(String, String)> {
        let mut working: Vec<(String, String)> = self
            .lanes
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(session, lane)| Some((session.clone(), lane.running.clone()?)))
            .collect();
        working.sort();
        working
    }

    /// The conversation a task belongs to: while it runs, from memory;
    /// otherwise from its run's row.
    pub(crate) fn session_of(&self, task: &str) -> Option<String> {
        if let Some(session) = self.task_sessions.lock().unwrap().get(task) {
            return Some(session.clone());
        }
        self.store.run_session(task).ok().flatten()
    }

    /// Frees a conversation's lane if `task` still holds it.
    pub(crate) fn release(&self, session: &str, task: &str) {
        if let Some(lane) = self.lanes.lock().unwrap().get_mut(session)
            && lane.running.as_deref() == Some(task)
        {
            lane.running = None;
        }
    }

    /// The scheduled run in progress beside the chat: (task, schedule).
    pub fn background_run(&self) -> Option<(String, i64)> {
        self.background
            .lock()
            .unwrap()
            .as_ref()
            .map(|b| (b.task.clone(), b.schedule))
    }

    /// Anything is running: a conversation's task or a scheduled run.
    pub fn is_working(&self) -> bool {
        !self.working().is_empty() || self.background.lock().unwrap().is_some()
    }

    /// The rules in force (policy.rs): recorded on runs and parked cards, and
    /// shown in `/status`. Follows the Browser actions and Auto approve settings.
    pub fn policy_version(&self) -> String {
        let settings = self.settings();
        self.policy.version(
            settings.allow_actions,
            settings.auto_approve,
            self.cfg.shell_tool,
            &self.cfg.run_as,
            &llm::KNOWN_MODELS,
        )
    }

    /// Stores a side effect to be done by the effects runner, and wakes it.
    pub fn enqueue(&self, kind: &str, key: &str, payload: &Value, due: i64) -> Result<()> {
        self.store.enqueue_effect(kind, key, payload, due)?;
        self.effects_changed.notify_one();
        Ok(())
    }

    /// The approval a parked task waits on, if any.
    pub fn waiting_on(&self) -> Option<String> {
        self.store.any_parked().ok().flatten().map(|p| p.id)
    }

    /// Whether a message is a memory command the signed-in user typed
    /// (run() recognises it the same way): it isn't a task for the model.
    pub(crate) fn is_memory_command(&self, text: &str, by: Option<&str>) -> bool {
        MemoryCommand::from_authenticated_chat(text, &self.cfg.listen, by).is_some()
    }

    /// Starts a task in a conversation whose lane the caller has reserved
    /// for it (`task`; sessions.rs).
    pub(crate) fn spawn_task(
        self: &Arc<Self>,
        task: String,
        text: String,
        by: Option<String>,
        session: String,
    ) {
        let agent = self.clone();
        tokio::spawn(async move { agent.run(task, text, by, None, session).await });
    }

    /// Runs a queued scheduled run to its end, beside the conversations (the
    /// background worker's, schedule.rs). It leaves their tasks, and the
    /// cards waiting there, alone.
    pub async fn run_queued(self: Arc<Self>, queued: QueuedRun, schedule: Schedule) {
        *self.background.lock().unwrap() = Some(Background {
            task: queued.id.clone(),
            schedule: schedule.id,
            attempt: queued.attempt,
        });
        self.background_cancel.send_replace(false);
        let prompt = schedule.prompt.clone();
        let session = schedule.session.clone();
        self.clone()
            .run(queued.id, prompt, None, Some(schedule), session)
            .await;
        *self.background.lock().unwrap() = None;
    }

    /// Whether `task` is the scheduled run in progress, on its first try.
    fn first_try(&self, task: &str) -> bool {
        self.background
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|b| b.task == task && b.attempt == 0)
    }

    /// A schedule was paused or cancelled: its queued runs are dropped and the
    /// one in progress is stopped. True if one was.
    pub fn stop_schedule_runs(&self, schedule: i64) -> bool {
        if let Err(err) = self.store.cancel_queued_runs(schedule) {
            tracing::error!(%err, schedule, "couldn't cancel the schedule's queued runs");
        }
        let running = self
            .background
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|b| b.schedule == schedule);
        if running {
            self.background_cancel.send_replace(true);
        }
        running
    }

    /// Queues a schedule's retry, due at `due`. False if the schedule is gone
    /// or paused, or already has a run queued or running.
    fn retry(&self, schedule: i64, due: i64) -> bool {
        let Ok(Some(schedule)) = self.store.schedule(schedule) else {
            return false;
        };
        if schedule.paused {
            return false;
        }
        let queued = self
            .store
            .enqueue_run(
                &short_id(),
                schedule.id,
                &schedule.prompt,
                &self.settings().model,
                &self.policy_version(),
                due,
                1,
            )
            .unwrap_or(false);
        if queued {
            self.runs_queued.notify_one();
        }
        queued
    }

    /// A note from the agent in a conversation, outside any task.
    pub fn say_in(&self, session: &str, text: &str) {
        let _ = self.store.add_history_in(session, "assistant", text);
        self.emit(
            "message",
            json!({"role": "assistant", "text": text, "session": session}),
        );
    }

    /// Gives up on every task of a conversation waiting for an approval.
    pub(crate) fn abandon_parked(&self, session: &str, answer: &str) -> bool {
        let mut any = false;
        // Bounded: abandon() removes each one (unless answered meanwhile).
        for _ in 0..16 {
            let Ok(Some(parked)) = self.store.parked_in(session) else {
                break;
            };
            self.abandon(parked, answer);
            any = true;
        }
        any
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

    /// The user's answer to the approval a task parked on: resumes the task
    /// in its conversation's lane at once, even with `MAX_PARALLEL_TASKS`
    /// working or the budget used up (they limit starting tasks, not
    /// finishing them).
    pub fn answer_approval(
        self: &Arc<Self>,
        id: &str,
        approved: bool,
        edited: Option<&str>,
        by: Option<String>,
    ) -> Result<(), StartError> {
        let Ok(Some(parked)) = self.store.parked(id) else {
            return Err(StartError::NotFound);
        };
        if approved
            && ((parked.card["kind"] == "shell" && parked.card["timeout"].is_null())
                || (parked.card["kind"] == "browser" && parked.card["binding"].is_null()))
        {
            self.abandon_because(parked, POLICY_CHANGED, Some("policy_changed"));
            return Err(StartError::NotFound);
        }
        if unix_now() - parked.created > APPROVAL_TTL_SECS {
            self.abandon(
                parked,
                "I stopped waiting for your approval: it came a day later, and the desktop has likely \
                 changed since. Ask me again if you still need this.",
            );
            return Err(StartError::NotFound);
        }
        // Parked under other rules: if today's would refuse it outright, it
        // expires rather than go ahead (docs/memory-v2.md §10). If they'd
        // still only ask, the user's answer stands.
        let current = self.policy_version();
        if approved && parked.policy_version.as_deref().is_some_and(|v| v != current) {
            let model = parked
                .checkpoint
                .as_deref()
                .and_then(|c| serde_json::from_str::<RunState>(c).ok())
                .map(|state| state.model);
            if let Some(why) = self.refused_now(&parked.card, model.as_deref()) {
                tracing::info!(approval = id, why, "parked under other rules: expired");
                self.abandon_because(parked, POLICY_CHANGED, Some("policy_changed"));
                return Err(StartError::NotFound);
            }
        }
        let mut state = parked.checkpoint.as_deref().and_then(load_parked_state);
        let mut card = parked.card.clone();
        // An edit that can't be used stops here: going ahead would run the
        // agent's words when the user approved their own.
        let edited = match edited.filter(|_| approved) {
            Some(text) => {
                let applied = state
                    .as_mut()
                    .and_then(|state| self.apply_edit(state, &mut card, text));
                if applied.is_none() {
                    return Err(StartError::NotFound);
                }
                applied
            }
            None => None,
        };
        let session = self.session_of(&parked.run).unwrap_or_else(main_session);
        {
            let mut lanes = self.lanes.lock().unwrap();
            let lane = lanes.entry(session.clone()).or_insert_with(Lane::new);
            // Only another card of the same conversation, answered first.
            if lane.running.is_some() {
                return Err(StartError::Busy);
            }
            lane.running = Some(parked.run.clone());
            lane.cancel.send_replace(false);
        }
        let (Some(mut state), Ok(true)) = (state, self.store.remove_pending(id)) else {
            self.release(&session, &parked.run);
            return Err(StartError::NotFound);
        };
        state.session = session.clone();
        self.task_sessions
            .lock()
            .unwrap()
            .insert(state.task.clone(), session.clone());
        // The event log records who decided (an audit trail).
        let mut resolved = json!({"task": parked.run, "id": id, "approved": approved, "by": by});
        if let Some(text) = &edited {
            resolved["edited"] = json!(true);
            resolved["text"] = json!(text);
        }
        self.emit("approval.resolved", resolved);
        let _ = self.store.set_run_status(&state.task, "running");
        self.emit("task.resumed", json!({"task": state.task}));
        tracing::info!(task = %state.task, approved, by = ?by, %session, "task resumed");
        if let Some(lane) = self.lanes.lock().unwrap().get_mut(&session) {
            lane.thread.open = true;
        }
        let agent = self.clone();
        let answer = Answer {
            approved,
            card,
            by: by.clone(),
            edited,
        };
        tokio::spawn(async move { agent.drive(state, Some(answer)).await });
        Ok(())
    }

    /// Why this card's text can't be changed to `text`, if it can't. A card
    /// that's gone isn't a problem here: answering it says so.
    pub fn edit_problem(&self, id: &str, text: &str) -> Option<&'static str> {
        let parked = self.store.parked(id).ok().flatten()?;
        if editable_text(&parked.card).is_none() {
            return Some("this card has no text to edit");
        }
        if text.trim().is_empty() {
            return Some("the text is empty: deny the card instead");
        }
        if parked.card["kind"] != "browser" {
            return None;
        }
        // The same limits apply_edit and type_text enforce, said here so the
        // card isn't reported as gone. Never names or quotes the key.
        if text.chars().count() > CARD_TEXT_MAX_CHARS {
            Some("the text is too long for a single browser action")
        } else if self.secrets.holds_a_value(text) {
            Some(
                "the text includes the value of a saved key, which the agent never types into a page: \
                 remove it, or use Take over to enter it yourself",
            )
        } else {
            None
        }
    }

    /// Puts what the user wrote into the tool call the task parked on, and into
    /// the card, as the call would make it, so the action still counts as the one
    /// they approved. The call then runs again from the top: a command that's
    /// refused outright stays refused, and edits cannot add saved credentials.
    /// Returns the text, or None if this card can't take an edit.
    fn apply_edit(&self, state: &mut RunState, card: &mut Value, text: &str) -> Option<String> {
        let (argument, field) = editable_text(card)?;
        if text.trim().is_empty() {
            return None;
        }
        let call = state.queue.first_mut()?;
        let mut args: Value = serde_json::from_str(call["function"]["arguments"].as_str()?).ok()?;
        args.as_object()?;
        let kind = card["kind"].as_str()?.to_string();
        // A schedule's instructions are trimmed when it's made.
        let text = if kind == "schedule" { text.trim() } else { text };
        args[argument] = json!(text);
        card[field] = match kind.as_str() {
            "browser" => {
                if text.chars().count() > CARD_TEXT_MAX_CHARS || self.secrets.holds_a_value(text) {
                    return None;
                }
                json!(text)
            }
            "schedule" => {
                // Without a name it would take one from the new instructions.
                args["name"] = card["name"].clone();
                json!(text.chars().take(SCHEDULE_PROMPT_MAX_CHARS).collect::<String>())
            }
            _ => json!(text),
        };
        call["function"]["arguments"] = json!(args.to_string());
        Some(text.to_string())
    }

    /// For the model: its own call still shows what it wrote.
    fn edited_note(&self, text: &str) -> String {
        format!(
            "\n\n(agentd: the user changed this before approving. What they approved: {})",
            self.secrets.redact(text)
        )
    }

    /// Why today's rules would refuse a parked card outright, if they would:
    /// a browser change with Browser actions off, a command with the shell
    /// tool off, or a task on a model that's no longer allowed.
    fn refused_now(&self, card: &Value, model: Option<&str>) -> Option<&'static str> {
        match card["kind"].as_str() {
            Some("browser") if !self.settings().allow_actions => Some("browser actions are off"),
            Some("shell")
                if card["command"]
                    .as_str()
                    .is_some_and(|command| split_sudo(command).0) =>
            {
                Some("system administration is available through SSM")
            }
            Some("shell") if !self.cfg.shell_tool => Some("the shell tool is off"),
            _ if model.is_some_and(|m| !llm::KNOWN_MODELS.contains(&m)) => Some("the model isn't allowed"),
            _ => None,
        }
    }

    /// Gives up on a parked task: its approval card expires and the task ends.
    fn abandon(&self, parked: Pending, answer: &str) {
        self.abandon_because(parked, answer, None);
    }

    /// `abandon`, saying why on the card (`policy_changed`).
    fn abandon_because(&self, parked: Pending, answer: &str, reason: Option<&str>) {
        if !self.store.remove_pending(&parked.id).unwrap_or(false) {
            return; // answered meanwhile
        }
        self.emit(
            "approval.resolved",
            json!({"task": parked.run, "id": parked.id, "approved": false, "expired": true, "reason": reason}),
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

    /// "I'm done" or "Skip" on an action-needed card: a message like any
    /// other in the card's conversation, which starts the resume or queues it.
    pub fn resolve_action(
        self: &Arc<Self>,
        id: &str,
        done: bool,
        by: Option<String>,
    ) -> Result<Posted, StartError> {
        let Ok(Some(action)) = self.store.action(id) else {
            return Err(StartError::NotFound);
        };
        let session = self.session_of(&action.run).unwrap_or_else(main_session);
        let text = if done {
            tools::RESUME_TEXT
        } else {
            tools::SKIP_TEXT
        };
        // An archived conversation's card still resumes there.
        let posted = self
            .post_to(&session, text.to_string(), by.clone(), true)
            .map_err(|_| StartError::NotFound)?;
        let _ = self.store.remove_pending(id);
        self.emit(
            "action.resolved",
            json!({"id": id, "choice": if done { "done" } else { "skip" }, "by": by, "session": session}),
        );
        Ok(posted)
    }

    fn save_image(&self, jpeg: &[u8]) -> Result<String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        std::fs::write(self.cfg.state_dir.join("images").join(format!("{id}.jpg")), jpeg)?;
        Ok(id)
    }

    // --- A task ------------------------------------------------------------------

    /// A task of `session`: a chat message (its lane reserved for `task_id`)
    /// or a scheduled run (beside the lanes).
    async fn run(
        self: Arc<Self>,
        task_id: String,
        text: String,
        by: Option<String>,
        scheduled: Option<Schedule>,
        session: String,
    ) {
        self.task_sessions
            .lock()
            .unwrap()
            .insert(task_id.clone(), session.clone());
        let settings = self.settings();
        let memory_command = scheduled
            .is_none()
            .then(|| MemoryCommand::from_authenticated_chat(&text, &self.cfg.listen, by.as_deref()))
            .flatten();
        // A key the user pasted into the chat after all: the model sees its name only.
        let text = memory_command
            .as_ref()
            .map_or_else(|| self.secrets.redact(&text).into_owned(), MemoryCommand::summary);
        // A scheduled run stands on its own: its prompt, without the chat. A
        // chat task sees its own conversation only.
        let history = match scheduled {
            Some(_) => Vec::new(),
            None => self.store.history_in(&session, HISTORY_LIMIT).unwrap_or_default(),
        };
        let (for_history, for_model) = match &scheduled {
            Some(schedule) => (
                format!("[Scheduled task “{}”] {text}", schedule.name),
                format!(
                    "(agentd) This is the scheduled task “{}” the user set up earlier ({}). They aren't \
                     watching, so do it and end with a short report they can read later in the chat.{} {}\n\n{text}",
                    schedule.name,
                    schedule::describe(schedule.cron.as_deref(), schedule.next_run, &schedule.timezone),
                    if self.cfg.memory_v2 {
                        format!(" {REPORT_OUTCOME_RULE}")
                    } else {
                        String::new()
                    },
                    if self.settings().auto_approve || schedule.auto_approve {
                        AUTO_APPROVE_RULE
                    } else {
                        UNATTENDED_RULE
                    },
                ),
            ),
            None => (text.clone(), text.clone()),
        };
        // A scheduled run's instructions reach the chat and the history with its
        // report, when it ends (finish): never in the middle of the user's
        // conversation.
        if scheduled.is_none() {
            self.open_thread(&session, &for_history);
        }
        let by_label = by
            .clone()
            .or_else(|| scheduled.as_ref().map(|_| "schedule".to_string()));
        // A queued run has its row already.
        let queued = scheduled.is_some()
            && self
                .store
                .begin_queued_run(&task_id, &settings.model, &self.policy_version())
                .unwrap_or(false);
        if !queued
            && let Err(err) = self.store.start_run_in(
                &session,
                &task_id,
                &text,
                by_label.as_deref(),
                &settings.model,
                &self.policy_version(),
                scheduled.as_ref().map(|s| s.id),
            )
        {
            tracing::error!(%err, "couldn't store the run");
        }
        match &scheduled {
            None => {
                self.emit(
                    "message",
                    json!({"role": "user", "text": text, "task": task_id, "by": by}),
                );
                self.emit("task.started", json!({"task": task_id, "model": settings.model}));
            }
            // Not task.started: the apps show that as the chat working.
            Some(schedule) => {
                self.emit(
                    "run.started",
                    json!({"task": task_id, "model": settings.model,
                           "schedule": {"id": schedule.id, "name": schedule.name}}),
                );
            }
        }
        tracing::info!(task = %task_id, model = %settings.model, by = ?by_label, %session, "task started");

        // The system prompt is written at each (re)start of the loop.
        let mut messages = vec![json!({"role": "system", "content": ""})];
        messages.extend(
            history
                .iter()
                .map(|h| json!({"role": h.role, "content": self.secrets.redact(&h.text)})),
        );
        messages.push(json!({"role": "user", "content": for_model}));
        let memory_query = if text == tools::RESUME_TEXT || text == tools::SKIP_TEXT {
            history
                .iter()
                .rev()
                .find(|h| h.role == "user")
                .map(|h| self.secrets.redact(&h.text).into_owned())
        } else {
            None
        };
        let messages_len = messages.len();
        let state = RunState {
            task: task_id,
            model: settings.model,
            messages,
            queue: Vec::new(),
            step: 0,
            tainted: false,
            reminded: false,
            loops: guard::LoopDetector::default(),
            usage: Usage::default(),
            seconds: 0.0,
            remember_asked: self.cfg.memory_v2
                && scheduled.is_none()
                && text != tools::RESUME_TEXT
                && text != tools::SKIP_TEXT
                && ASKS_TO_REMEMBER.is_match(&text),
            remembered: false,
            nudged: false,
            memory_query,
            reported: None,
            empty_retries: 0,
            plan: Vec::new(),
            review: ReviewClock {
                // From the request on: the history before it isn't this task's work.
                at_message: messages_len - 1,
                ..ReviewClock::default()
            },
            schedule_id: scheduled.as_ref().map(|s| s.id),
            scheduled: scheduled.map(|s| s.name),
            request: text,
            session,
        };
        if let Some(command) = memory_command {
            match self.apply_memory_command(&state.task, command, by.as_deref()) {
                Ok(answer) => self.finish(&state, &answer, "done"),
                Err(err) => {
                    tracing::error!(%err, "couldn't apply the memory command");
                    self.finish(&state, "The memory command could not be completed.", "error");
                }
            }
            self.release(&state.session, &state.task);
            self.pump();
            return;
        }
        self.drive(state, None).await;
    }

    /// Runs a task until it finishes or parks on an approval, then frees its
    /// conversation's lane and starts what's queued (a scheduled run frees
    /// the background worker by returning).
    async fn drive(self: &Arc<Self>, mut state: RunState, answer: Option<Answer>) {
        let started = Instant::now();
        let result = self.run_loop(&mut state, answer).await;
        state.seconds += started.elapsed().as_secs_f64();
        let chat = state.scheduled.is_none();
        match result {
            Ok(Outcome::Parked(park)) if chat => match self.save_parked(&state, &park) {
                Ok(()) => {
                    // Free the lane before asking, so an answer can resume it
                    // at once; its queue waits behind the card.
                    self.close_thread(&state.session, None);
                    self.release(&state.session, &state.task);
                    self.ask(&state.task, park);
                    self.task_sessions.lock().unwrap().remove(&state.task);
                    self.pump();
                    return;
                }
                Err(err) => self.finish(&state, &format!("Something went wrong: {err:#}"), "error"),
            },
            // A scheduled run never parks (approve); if one did, it ends here.
            Ok(Outcome::Parked(_)) => self.finish(
                &state,
                "Something went wrong: a scheduled run asked for an approval.",
                "error",
            ),
            Ok(Outcome::Finished(answer, status)) => self.finish(&state, &answer, status),
            Err(err) => self.finish(&state, &format!("Something went wrong: {err:#}"), "error"),
        }
        if chat {
            self.release(&state.session, &state.task);
            self.pump();
        }
    }

    /// A conversation's task starts: its question goes into its history, and
    /// the reports of scheduled runs that end meanwhile wait for its answer.
    fn open_thread(&self, session: &str, question: &str) {
        let mut lanes = self.lanes.lock().unwrap();
        let lane = lanes.entry(session.to_string()).or_insert_with(Lane::new);
        if let Err(err) = self.store.add_history_in(session, "user", question) {
            tracing::error!(%err, "couldn't store the message");
        }
        lane.thread.open = true;
    }

    /// The task's answer (none when it parks), then the reports that came in
    /// meanwhile.
    fn close_thread(&self, session: &str, answer: Option<&str>) {
        let mut lanes = self.lanes.lock().unwrap();
        let lane = lanes.entry(session.to_string()).or_insert_with(Lane::new);
        if let Some(answer) = answer
            && let Err(err) = self.store.add_history_in(session, "assistant", answer)
        {
            tracing::error!(%err, "couldn't store the answer");
        }
        lane.thread.open = false;
        for (role, text) in std::mem::take(&mut lane.thread.held) {
            if let Err(err) = self.store.add_history_in(session, role, &text) {
                tracing::error!(%err, "couldn't store a scheduled run's report");
            }
        }
    }

    /// A scheduled run's report, for the model's later turns in its
    /// conversation: now, or after that conversation's answer if a task of
    /// it is in progress.
    fn add_report_history(&self, session: &str, instructions: String, report: &str) {
        let mut lanes = self.lanes.lock().unwrap();
        let lane = lanes.entry(session.to_string()).or_insert_with(Lane::new);
        if lane.thread.open {
            lane.thread.held.push(("user", instructions));
            lane.thread.held.push(("assistant", report.to_string()));
            return;
        }
        for (role, text) in [("user", instructions.as_str()), ("assistant", report)] {
            if let Err(err) = self.store.add_history_in(session, role, text) {
                tracing::error!(%err, "couldn't store a scheduled run's report");
            }
        }
    }

    fn save_parked(&self, state: &RunState, park: &Park) -> Result<()> {
        let mut checkpoint = serde_json::to_value(state)?;
        scrub_memory_checkpoint(&mut checkpoint);
        self.store.park(
            &park.id,
            &state.task,
            &park.card,
            &checkpoint.to_string(),
            &self.policy_version(),
        )?;
        self.store.set_run_status(&state.task, "waiting")
    }

    /// Shows a parked task's approval card.
    fn ask(&self, task_id: &str, park: Park) {
        tracing::info!(task = %task_id, approval = %park.id, "waiting for approval");
        self.notify_card(task_id, &park.card);
        let mut card = park.card;
        if let Some(fields) = card.as_object_mut() {
            fields.remove("binding");
        }
        self.emit("approval.requested", card);
        self.emit("task.paused", json!({"task": task_id, "approval": park.id}));
    }

    /// The answer, to the chat and the history, and the task's end.
    fn finish(&self, state: &RunState, answer: &str, status: &str) {
        let task_id = &state.task;
        self.step_seqs.lock().unwrap().remove(task_id);
        self.record_usage(state);
        let cost = state.usage.cost(&state.model);
        let seconds = state.seconds.round();
        // A scheduled run that failed (the model or the network, most likely)
        // is tried once more, a little later, and only the retry's report is
        // shown.
        if let (Some(schedule), "error") = (state.schedule_id, status)
            && self.first_try(task_id)
            && self.store.run_has_no_actions(task_id).unwrap_or(false)
        {
            if let Err(err) = self.store.finish_run(task_id, status, state.step, cost, answer) {
                tracing::error!(%err, "couldn't store the run's end");
            } else if self.retry(schedule, unix_now() + RETRY_AFTER_SECS) {
                tracing::warn!(task = %task_id, schedule, "scheduled run failed: trying again");
                self.emit(
                    "run.finished",
                    json!({"task": task_id, "status": status, "steps": state.step, "seconds": seconds,
                           "model": state.model, "cost": cost, "retry": true}),
                );
                self.task_sessions.lock().unwrap().remove(task_id);
                return;
            }
        }
        match &state.scheduled {
            None => self.close_thread(&state.session, Some(answer)),
            Some(name) => {
                self.add_report_history(
                    &state.session,
                    format!("[Scheduled task “{name}”] {}", state.request),
                    answer,
                );
                // The report, whole: its instructions, then its answer below.
                self.emit(
                    "message",
                    json!({"role": "user", "text": state.request, "task": task_id, "by": null,
                           "schedule": {"id": state.schedule_id, "name": name}}),
                );
            }
        }
        // A scheduled run that found nothing, or that was stopped because its
        // schedule was paused or cancelled: a quiet row in the chat.
        let quiet = state.reported.as_ref().is_some_and(|r| r.outcome == "nothing")
            || (state.scheduled.is_some() && status == "cancelled");
        self.emit(
            "message",
            json!({"role": "assistant", "text": answer, "task": task_id, "quiet": quiet.then_some(true)}),
        );
        self.emit(
            if state.scheduled.is_some() {
                "run.finished"
            } else {
                "task.finished"
            },
            json!({
                "task": task_id, "status": status, "steps": state.step, "seconds": seconds,
                "model": state.model, "usage": state.usage, "cost": cost,
                "outcome": state.reported.as_ref().map(|r| &r.outcome),
            }),
        );
        let consolidate = state.scheduled.is_none()
            && self.cfg.memory_v2
            && crate::consolidate::mode(&self.store) == crate::consolidate::Mode::Auto;
        if let Err(err) =
            self.store
                .finish_run_and_enqueue(task_id, status, state.step, cost, answer, consolidate)
        {
            tracing::error!(%err, "couldn't store the run's end");
        } else if consolidate {
            self.effects_changed.notify_one();
        }
        self.notify_finished(
            task_id,
            status,
            state.seconds,
            answer,
            state.scheduled.as_deref(),
            state.reported.as_ref().map(|r| r.summary.as_str()),
            quiet,
        );
        let outcome = state.reported.as_ref().map(|r| r.outcome.as_str());
        tracing::info!(task = %task_id, status, steps = state.step, seconds, outcome, "task finished");
        if self.cfg.memory_v2 && state.scheduled.is_some() && status == "done" && state.reported.is_none() {
            // Counted, so the scheduled-run prompt can be tuned.
            tracing::info!(task = %task_id, "report_outcome_missing");
        }
        self.task_sessions.lock().unwrap().remove(task_id);
    }

    /// What a task has cost so far, on its run's row after each model call
    /// (supervisor checks and wrap-ups too), so running tasks count; then
    /// the budget's alerts (usage.rs).
    fn record_usage(&self, state: &RunState) {
        let cost = state.usage.cost(&state.model);
        if let Err(err) = self
            .store
            .record_usage(&state.task, cost, &state.usage, state.step)
        {
            tracing::error!(%err, task = %state.task, "couldn't record the run's usage");
        }
        self.check_budget();
    }

    async fn run_loop(&self, state: &mut RunState, mut answer: Option<Answer>) -> Result<Outcome> {
        // Working time: what the task had, plus this stretch (drive adds it on).
        let loop_started = Instant::now();
        let now = self.user_clock().now();
        // The date and time as of now: a parked task may resume much later.
        let (standing, hints) = if self.cfg.memory_v2 {
            let request = state.memory_query.as_deref().unwrap_or(&state.request);
            let recall = self.recall(&state.task, request, state.schedule_id);
            (recall.standing, recall.hints)
        } else {
            self.memory_prompt()
        };
        let prompt = tools::system_prompt(
            self.cfg.shell_tool,
            &now,
            &standing,
            &self.skills.listing(),
            &self.keys_prompt(),
            self.cfg.memory_v2,
        );
        state.messages[0] = json!({"role": "system", "content": prompt});
        let schemas = tools::schemas(
            self.cfg.shell_tool,
            !self.skills.is_empty(),
            self.cfg.memory_v2,
            self.cfg.memory_v2 && state.scheduled.is_some(),
        );
        // The user answered a stuck card (review): keep going with the
        // supervisor's advice, or stop with a summary of where it got to
        // (without the browser: it's no longer needed).
        if answer.as_ref().is_some_and(|a| a.card["kind"] == "stuck") {
            let answer = answer.take().expect("checked");
            let (reason, advice) = state.review.stuck.take().unwrap_or_default();
            if !answer.approved {
                return Ok(self
                    .wrap_up(
                        state,
                        &schemas,
                        &format!("The user stopped this task after a check found it stuck: {reason}"),
                        format!("Stopped: {reason}"),
                        "stuck",
                    )
                    .await);
            }
            let advice = if advice.is_empty() {
                String::new()
            } else {
                format!(" The check's advice: {advice}")
            };
            state.messages.push(json!({"role": "user", "content": format!(
                "(agentd) A check on your progress found you may be stuck ({reason}), and the user chose to \
                 keep going.{advice}"
            )}));
            let (step, secs, messages) = (state.step, state.seconds, state.messages.len());
            state.review.checked(step, secs, messages);
        }
        let background = state.scheduled.is_some();
        // Connected when a tool first needs it, so a task that doesn't
        // browse doesn't need Chrome.
        let mut browser = Browser::new(self.cdp.clone());
        if background {
            // A window of its own: never a tab a conversation works in.
            browser.temporary_window();
        } else if state.session == MAIN {
            browser.shared(self.store.setting("tab_target")?, self.foreign_tabs());
        } else {
            // Its own window, kept from its earlier tasks.
            let remembered = self.store.session(&state.session)?.and_then(|s| s.tab_target);
            browser.own_window(remembered);
        }
        // One receiver to check between calls, a second to interrupt model
        // calls and browser actions.
        let (mut cancel, mut stop) = if background {
            (
                self.background_cancel.subscribe(),
                self.background_cancel.subscribe(),
            )
        } else {
            let mut lanes = self.lanes.lock().unwrap();
            let lane = lanes.entry(state.session.clone()).or_insert_with(Lane::new);
            (lane.cancel.subscribe(), lane.cancel.subscribe())
        };
        let task_id = state.task.clone();
        // The tab last remembered for the conversation, saved again when it changes.
        let mut tab = None;

        let outcome = 'task: loop {
            // The calls of the model's last reply; after a resume, from the parked one on.
            let mut nudge = None;
            while let Some(call) = state.queue.first().cloned() {
                if *cancel.borrow() {
                    break 'task Outcome::Finished("Stopped.".into(), "cancelled");
                }
                // The user's answer belongs to the first call: the one parked on.
                let resumed = answer.take();
                let edited_note = resumed
                    .as_ref()
                    .and_then(|answer| answer.edited.as_deref())
                    .map(|text| self.edited_note(text))
                    .unwrap_or_default();
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
                    Some(command) if name == "run_shell" && guard::is_piped_install(command) => {
                        Some(guard::PIPED_INSTALL_REFUSAL.to_string())
                    }
                    Some(command) if name == "run_shell" && guard::is_shell_schedule(command) => {
                        Some(guard::SHELL_SCHEDULE_REFUSAL.to_string())
                    }
                    // Not even with approval: the egress controls.
                    Some(command) if name == "run_shell" && guard::touches_egress_policy(command) => {
                        Some(guard::EGRESS_REFUSAL.to_string())
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
                    let content = format!("{refusal}{edited_note}");
                    state
                        .messages
                        .push(json!({"role": "tool", "tool_call_id": call["id"], "content": content}));
                    continue;
                }
                // A resumed call was shown when it was first made.
                if resumed.is_none() {
                    tracing::info!(task = %task_id, step, tool = %name, "tool call");
                    let mut event = json!({"task": task_id, "step": step, "tool": name});
                    if !is_memory_tool(&name) {
                        event["args"] = json!(truncate(&raw, 300));
                    }
                    let seq = self.emit("step", event);
                    self.step_seqs
                        .lock()
                        .unwrap()
                        .insert(task_id.clone(), seq.unwrap_or(0));
                }
                if name == "remember" {
                    state.remembered = true;
                }
                // A scheduled run's last word (memory v2): ends the task.
                if name == "report_outcome" && state.scheduled.is_some() && self.cfg.memory_v2 {
                    let outcome = args["outcome"].as_str().unwrap_or_default();
                    let summary = args["summary"].as_str().unwrap_or_default().trim();
                    if matches!(outcome, "result" | "nothing" | "blocked") {
                        state.reported = Some(schedule::Reported {
                            outcome: outcome.to_string(),
                            summary: schedule::clip(summary, 180),
                        });
                        // The answer stays the model's own text when it wrote one.
                        let text = state
                            .messages
                            .iter()
                            .rev()
                            .find(|m| m["role"] == "assistant")
                            .and_then(|m| m["content"].as_str())
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                            .map(String::from);
                        let answer = text.unwrap_or_else(|| match outcome {
                            "nothing" => "Nothing new.".to_string(),
                            _ => summary.to_string(),
                        });
                        break 'task Outcome::Finished(answer, "done");
                    }
                    state.queue.remove(0);
                    state.messages.push(json!({
                        "role": "tool", "tool_call_id": call["id"],
                        "content": "error: outcome must be result, nothing or blocked",
                    }));
                    continue;
                }
                if name == "needs_user" {
                    let reason = args["reason"]
                        .as_str()
                        .unwrap_or("Please take over the desktop.")
                        .to_string();
                    self.action_needed(&mut browser, &task_id, &reason).await;
                    break 'task Outcome::Finished(format!("I need you: {reason}"), "needs_user");
                }
                if name == "update_plan" {
                    state.queue.remove(0);
                    let content = match plan::parse(&args) {
                        Ok(items) => {
                            state.plan = items;
                            // The web app's checklist: a scheduled run's isn't watched.
                            if !background {
                                self.emit("plan", json!({"task": task_id, "items": state.plan}));
                            }
                            let (done, total) = plan::progress(&state.plan);
                            format!("ok: plan saved ({done} of {total} done)")
                        }
                        Err(why) => format!("error: {why}"),
                    };
                    state
                        .messages
                        .push(json!({"role": "tool", "tool_call_id": call["id"], "content": content}));
                    continue;
                }
                let result = if args.is_object() {
                    let call = self.call_tool(
                        &mut browser,
                        &task_id,
                        &name,
                        &args,
                        state.tainted,
                        state.schedule_id.or(state.scheduled.as_ref().map(|_| 0)),
                        resumed,
                        &mut cancel,
                    );
                    // Shell commands stop their own unit when stopped; anything
                    // else is simply dropped.
                    let done = if name == "run_shell" {
                        call.await
                    } else {
                        tokio::select! {
                            done = call => done,
                            _ = stop.wait_for(|stopped| *stopped) => {
                                break 'task Outcome::Finished("Stopped.".into(), "cancelled");
                            }
                        }
                    };
                    match done {
                        Ok(result) => result,
                        Err(err) => match err.downcast::<Park>() {
                            Ok(park) => break 'task Outcome::Parked(park),
                            Err(err) => format!("error: {err:#}"),
                        },
                    }
                } else {
                    "error: arguments must be a JSON object".to_string()
                };
                // A saved key's value never reaches the model.
                let result = self.secrets.redact(&result).into_owned();
                state.queue.remove(0);
                let repeat = state.loops.record(&name, &raw, &result);
                if guard::carries_outside_content(&name) {
                    state.tainted = true;
                }
                let result = guard::label_untrusted(&name, result);
                // What the page did on its own meanwhile (a dialog agentd
                // answered, a file picker it held back), whatever the tool:
                // page text, so it's labelled and taints like a read.
                let notes = browser.take_notes();
                let result = if notes.is_empty() {
                    result
                } else {
                    state.tainted = true;
                    tracing::info!(task = %task_id, notes = notes.len(), "the page acted on its own");
                    let notes = notes
                        .iter()
                        .map(|n| format!("(agentd: {n}.)"))
                        .collect::<Vec<_>>();
                    format!(
                        "{result}\n\n{}",
                        guard::label_untrusted("read_text", notes.join("\n"))
                    )
                };
                // After the label: these are the user's words, not outside content.
                let content = format!("{result}{edited_note}");
                state
                    .messages
                    .push(json!({"role": "tool", "tool_call_id": call["id"], "content": content}));
                // The conversation's tab, as soon as it has one: a card parked
                // on it, or its next task, finds it again.
                if !background && browser.current_target() != tab {
                    tab = browser.current_target();
                    if let Some(target) = &tab {
                        self.set_tab_target(&state.session, target);
                    }
                }
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
            // No cap on steps: every so often the supervisor checks on the task.
            let active = state.seconds + loop_started.elapsed().as_secs_f64();
            if self.review_due(state, active) {
                let outcome = tokio::select! {
                    outcome = self.review(state, &schemas, active) => outcome,
                    _ = stop.wait_for(|stopped| *stopped) => {
                        break 'task Outcome::Finished("Stopped.".into(), "cancelled");
                    }
                };
                if let Some(outcome) = outcome {
                    break 'task outcome;
                }
            }

            // The model's next turn.
            state.step += 1;
            let step = state.step;
            let cut = guard::enforce_budget(&mut state.messages, guard::TOOL_OUTPUT_BUDGET);
            if cut > 0 {
                tracing::info!(task = %task_id, step, cut, "trimmed old tool output from the context");
            }
            let max_tokens = turn_max_tokens(state.empty_retries);
            let mut messages = state.messages.clone();
            if !hints.is_empty() {
                messages.insert(
                    1,
                    json!({
                        "role": "assistant",
                        "content": format!("Earlier notes and reports as quoted data:\n{}", json!(hints)),
                    }),
                );
            }
            let mut request = json!({
                "model": state.model, "messages": messages, "tools": schemas,
                "tool_choice": "auto", "max_tokens": max_tokens,
            });
            // A stop interrupts the model call rather than waiting for its answer.
            let reply = tokio::select! {
                reply = self.llm.chat(&request) => reply,
                _ = stop.wait_for(|stopped| *stopped) => {
                    break 'task Outcome::Finished("Stopped.".into(), "cancelled");
                }
            };
            // More room than the model or the account allows: the usual limit, then.
            let reply = match reply {
                Err(err) if max_tokens > TURN_MAX_TOKENS => {
                    tracing::warn!(task = %task_id, step, max_tokens, err = %format!("{err:#}"),
                        "a larger reply was refused: trying the usual limit");
                    request["max_tokens"] = json!(TURN_MAX_TOKENS);
                    tokio::select! {
                        reply = self.llm.chat(&request) => reply?,
                        _ = stop.wait_for(|stopped| *stopped) => {
                            break 'task Outcome::Finished("Stopped.".into(), "cancelled");
                        }
                    }
                }
                reply => reply?,
            };
            state.usage.add(reply.get("usage"));
            self.record_usage(state);
            let message = reply["choices"][0]["message"].clone();
            // Back into the conversation, with reasoning_content for the
            // models that want it (GLM, Kimi) and without for the rest.
            state.messages.push(llm::for_history(&message, &state.model));
            let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
            if calls.is_empty() && is_empty_reply(&message) {
                // No text and no call: the thinking used up the reply. Ask
                // again, with more room each time; then end with a summary.
                let finish = reply["choices"][0]["finish_reason"].as_str().unwrap_or("");
                if state.empty_retries < EMPTY_RETRIES {
                    state.empty_retries += 1;
                    tracing::info!(task = %task_id, step, finish, retry = state.empty_retries,
                        "empty reply: asking again");
                    state
                        .messages
                        .push(json!({"role": "user", "content": EMPTY_REPLY}));
                    continue;
                }
                tracing::warn!(task = %task_id, step, finish, "empty replies: ending with a summary");
                break 'task self
                    .wrap_up(
                        state,
                        &schemas,
                        "Your last replies were cut off before you wrote anything.",
                        "The model's replies kept getting cut off before it wrote anything, so I stopped. Ask me \
                         again, or pick another model in Settings."
                            .into(),
                        // Not "error": a scheduled run would be tried again and cut off again.
                        "stuck",
                    )
                    .await;
            }
            state.empty_retries = 0;
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
                // Memory v2: asked to remember, and nothing saved: one reminder.
                if state.remember_asked && !state.remembered && !state.nudged {
                    state.nudged = true;
                    tracing::info!(task = %task_id, step, "nudged to remember");
                    state
                        .messages
                        .push(json!({"role": "user", "content": REMEMBER_NUDGE}));
                    continue;
                }
                let answer = message["content"].as_str().unwrap_or_default().trim().to_string();
                break 'task Outcome::Finished(answer, "done");
            }
            state.queue = calls;
        };
        if background {
            // Its window stays open for the user only when it needs them
            // (needs_user), and "I'm done" carries on there, in its
            // conversation; otherwise it's closed when the browser is dropped.
            if matches!(outcome, Outcome::Finished(_, "needs_user"))
                && let Some(target) = browser.keep_own_window()
            {
                self.set_tab_target(&state.session, &target);
            }
        } else if let Some(target) = browser.current_target()
            && tab.as_ref() != Some(&target)
        {
            self.set_tab_target(&state.session, &target);
        }
        Ok(outcome)
    }

    /// Remembers a conversation's tab: `main`'s in settings, another's on
    /// its row.
    fn set_tab_target(&self, session: &str, target: &str) {
        let saved = if session == MAIN {
            self.store.set_setting("tab_target", target)
        } else {
            self.store.set_session_tab(session, Some(target))
        };
        if let Err(err) = saved {
            tracing::warn!(%err, %session, "couldn't remember the conversation's tab");
        }
    }

    /// A tab of each other conversation's window, which `main` leaves alone.
    fn foreign_tabs(&self) -> Vec<String> {
        self.store
            .session_tabs()
            .unwrap_or_default()
            .into_iter()
            .map(|(_, tab)| tab)
            .collect()
    }

    /// The task has to end before it's done (`why`, for the model): one last
    /// turn without tools, so the user hears what was done and what's left
    /// rather than just "stopped". `fallback` if that turn gives nothing.
    async fn wrap_up(
        &self,
        state: &mut RunState,
        schemas: &Value,
        why: &str,
        fallback: String,
        status: &'static str,
    ) -> Outcome {
        state
            .messages
            .push(json!({"role": "user", "content": tools::wrap_up(why)}));
        guard::enforce_budget(&mut state.messages, guard::TOOL_OUTPUT_BUDGET);
        let request = json!({
            "model": state.model, "messages": state.messages, "tools": schemas,
            "tool_choice": "none", "max_tokens": TURN_MAX_TOKENS,
        });
        let answer = match self.llm.chat(&request).await {
            Ok(reply) => {
                state.usage.add(reply.get("usage"));
                self.record_usage(state);
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
        Outcome::Finished(answer, status)
    }

    /// Whether the supervisor should check on the task now: enough working
    /// time or model turns since its last check.
    fn review_due(&self, state: &RunState, active: f64) -> bool {
        let (steps, secs) = (self.cfg.review_every_steps, self.cfg.review_every_secs);
        (steps > 0 && state.step.saturating_sub(state.review.at_step) >= steps)
            || (secs > 0 && active - state.review.at_secs >= secs as f64)
    }

    /// The supervisor's check (supervise.rs). None: carry on, after a nudge
    /// with its advice in the conversation. Some: a chat task parks on a stuck
    /// card for the user (never answered by Auto approve: it's about whether to
    /// keep spending, not a permission), and a scheduled run ends with a summary.
    async fn review(&self, state: &mut RunState, schemas: &Value, active: f64) -> Option<Outcome> {
        let task_id = state.task.clone();
        let minutes = (active / 60.0).round() as u64;
        let digest = supervise::Digest {
            request: &state.request,
            plan: &state.plan,
            steps: state.step,
            minutes,
            steps_since: state.step.saturating_sub(state.review.at_step),
            since: state.messages.get(state.review.at_message..).unwrap_or_default(),
            previous: state.review.previous.as_deref(),
        }
        .text();
        let review = match self.llm.chat(&supervise::request(&state.model, &digest)).await {
            Ok(reply) => {
                state.usage.add(reply.get("usage"));
                self.record_usage(state);
                supervise::parse(&reply)
            }
            Err(err) => {
                tracing::warn!(task = %task_id, err = %format!("{err:#}"), "the supervisor's check failed");
                None
            }
        };
        let review = match review {
            Some(review) => review,
            None => {
                state.review.failures += 1;
                self.emit("review", json!({"task": task_id, "verdict": "failed"}));
                // Tried again before the next turn; twice in a row, it counts as stuck.
                if state.review.failures < REVIEW_FAILURES {
                    return None;
                }
                supervise::Review {
                    verdict: Verdict::Stuck,
                    reason: "The check on this task's progress failed twice in a row.".into(),
                    advice: String::new(),
                }
            }
        };
        let reason = if review.reason.is_empty() {
            "It doesn't seem to be getting anywhere.".to_string()
        } else {
            review.reason
        };
        tracing::info!(task = %task_id, step = state.step, minutes, verdict = review.verdict.as_str(), "supervisor's check");
        self.emit(
            "review",
            json!({"task": task_id, "verdict": review.verdict.as_str(), "reason": reason}),
        );
        let (step, messages) = (state.step, state.messages.len());
        state.review.checked(step, active, messages);
        match review.verdict {
            Verdict::Progressing => {
                state.review.previous = None;
                None
            }
            Verdict::Nudge => {
                let advice = if review.advice.is_empty() {
                    "Step back and try a different approach.".to_string()
                } else {
                    review.advice
                };
                state.review.previous = Some(format!("nudge: {advice}"));
                state.messages.push(json!({"role": "user", "content": format!(
                    "(agentd) A check on your progress: {reason} Try this: {advice}"
                )}));
                None
            }
            Verdict::Stuck if state.scheduled.is_some() => Some(
                self.wrap_up(
                    state,
                    schemas,
                    &format!("A check on your progress found you stuck: {reason}"),
                    format!("Stopped: {reason}"),
                    "stuck",
                )
                .await,
            ),
            Verdict::Stuck => {
                state.review.previous = Some(format!("stuck: {reason}"));
                state.review.stuck = Some((reason.clone(), review.advice));
                let (done, total) = plan::progress(&state.plan);
                let id = short_id();
                let card = json!({
                    "kind": "stuck", "id": id, "task": task_id, "reason": reason,
                    "minutes": minutes, "steps": step,
                    "plan": (total > 0).then(|| json!({"done": done, "total": total})),
                });
                Some(Outcome::Parked(Park { id, card }))
            }
        }
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
        // Some for a scheduled run (its schedule id; 0 if unknown).
        schedule: Option<i64>,
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
        // A scheduled run works in one tab of a window of its own.
        if browser.is_temporary() && matches!(name, "list_tabs" | "switch_tab") {
            return Ok(
                "not done: a scheduled run works in a browser window of its own, not the \
                       user's tabs; use open_url there"
                    .into(),
            );
        }
        // The browser connects (or opens the conversation's window) the
        // first time a tool reads or loads a page. Inputs connect after
        // their own checks, which need no Chrome.
        if matches!(
            name,
            "list_tabs"
                | "switch_tab"
                | "open_url"
                | "read_text"
                | "snapshot"
                | "wait_for"
                | "show_screenshot"
        ) {
            browser.ensure().await?;
        }
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
                // Navigation isn't an action (§6.13): a task that only reads
                // pages can still be retried and gets no work-log entry.
                browser
                    .open_url(url, args["new_tab"].as_bool().unwrap_or(false))
                    .await
            }
            "read_text" => browser.read_text(max_chars).await,
            "snapshot" => browser.snapshot(max_chars).await,
            "click" | "type_text" | "press_key" => {
                if !self.settings().allow_actions {
                    return Ok(
                        "blocked: browser inputs are disabled. Use Take over if an action is needed.".into(),
                    );
                }
                if schedule.is_some() {
                    return Ok(
                        "not done: browser inputs require approval and a scheduled run can't ask.".into(),
                    );
                }
                browser.ensure().await?;

                let value = match name {
                    "type_text" => {
                        if args["submit"] == true {
                            return Ok(
                                "not typed: submitting is a separate action and needs its own approval."
                                    .into(),
                            );
                        }
                        let value = required("text")?;
                        if value.chars().count() > CARD_TEXT_MAX_CHARS || self.secrets.holds_a_value(value) {
                            return Ok("not typed: the full text must fit on the card and cannot include a saved key. Use Take over if needed.".into());
                        }
                        Some(value)
                    }
                    "press_key" => Some(required("key")?),
                    _ => None,
                };
                let element = if name == "press_key" {
                    None
                } else {
                    let visible_text = if name == "click" { text("text") } else { None };
                    Some(
                        browser
                            .find(number("ref"), text("role"), text("name"), visible_text)
                            .await?,
                    )
                };
                let place = match browser.place_of(element.as_ref()).await {
                    Ok(place) => place,
                    Err(err) => {
                        return Ok(format!(
                            "not done: couldn't verify the browser target ({err:#}). Use Take over if needed."
                        ));
                    }
                };
                if place.protected
                    || guard::check_url(&place.url).is_err()
                    || place
                        .destination
                        .as_deref()
                        .is_some_and(|url| guard::check_url(url).is_err())
                {
                    return Ok("not done: this browser target requires human Take over.".into());
                }
                let (action, reason) = match name {
                    "type_text" => ("type", "writes on the page"),
                    "press_key" => ("key", "may trigger an action on the page"),
                    _ => ("click", "may change or send something"),
                };
                let label = element
                    .as_ref()
                    .map_or(place.label.as_str(), |element| &element.label);
                let role = element
                    .as_ref()
                    .and_then(|element| element.role.as_ref())
                    .or(place.role.as_ref());
                let mut card = json!({
                    "action": action, "target": label, "role": role,
                    "site": reqwest::Url::parse(&place.url).ok().and_then(|url| url.host_str().map(String::from)),
                    "url": place.url, "page": place.title, "around": place.around,
                    "destination": place.destination, "binding": place.binding,
                    "summary": text("summary").and_then(clip_summary),
                });
                if action == "type" {
                    card["text"] = json!(value);
                    card["editable"] = json!(true);
                } else if action == "key" {
                    card["key"] = json!(value);
                }
                if resumed.is_none() {
                    self.explain_card(browser, &mut card).await;
                }
                if let Err(refusal) = self.gate_browser_action(task_id, reason, card, resumed, false)? {
                    return Ok(refusal);
                }
                if !self.settings().allow_actions {
                    return Ok("blocked: browser inputs were disabled.".into());
                }
                self.acted(task_id)?;
                match (action, element.as_ref(), value) {
                    ("type", Some(element), Some(value)) => browser.type_text(element, value, &place).await,
                    ("key", _, Some(key)) => browser.press_key(key, &place).await,
                    (_, Some(element), _) => browser.click(element, &place).await,
                    _ => unreachable!("browser action arguments checked above"),
                }
            }
            "remember" | "forget" => self.memory_tool(task_id, name, args, schedule),
            "search_memory" if self.cfg.memory_v2 => self.memory_tool(task_id, name, args, schedule),
            "schedule_task" => {
                if schedule.is_some() {
                    return Ok(
                        "not scheduled: a scheduled run can't set up schedules; tell the user to ask \
                               for it in the chat"
                            .into(),
                    );
                }
                self.acted(task_id)?;
                self.schedule_task(task_id, args, tainted, resumed)
            }
            "list_schedules" => {
                let schedules = self.store.schedules()?;
                if schedules.is_empty() {
                    return Ok("no schedules".into());
                }
                Ok(schedules
                    .iter()
                    .map(|s| {
                        format!(
                            "#{} “{}”: {}{}; next run {}. Prompt: {}\n",
                            s.id,
                            s.name,
                            schedule::describe(s.cron.as_deref(), s.next_run, &s.timezone),
                            if s.paused { " (paused)" } else { "" },
                            schedule::local(s.next_run, &s.timezone),
                            s.prompt,
                        )
                    })
                    .collect())
            }
            "cancel_schedule" => {
                if schedule.is_some() {
                    return Ok("not cancelled: scheduled runs cannot change schedules.".into());
                }
                let id = number("id").ok_or_else(|| anyhow!("id is required"))?;
                self.acted(task_id)?;
                if self.store.delete_schedule(id)? {
                    self.stop_schedule_runs(id);
                    self.emit("schedule.deleted", json!({"task": task_id, "id": id}));
                    Ok(format!("cancelled schedule #{id}"))
                } else {
                    Ok(format!("there's no schedule #{id}"))
                }
            }
            "read_skill" => {
                let skill = required("name")?;
                Ok(self.skills.read(skill).map(String::from).unwrap_or_else(|| {
                    format!("no skill named {skill}; the skills are listed in your instructions")
                }))
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
                let asked = asked_secrets(args);
                // "Run without asking", read now: turning it off applies at once.
                let auto_approve = schedule
                    .filter(|id| *id > 0)
                    .and_then(|id| self.store.schedule(id).ok().flatten())
                    .is_some_and(|s| s.auto_approve);
                self.run_shell(
                    task_id,
                    required("command")?,
                    text("summary"),
                    &asked,
                    timeout,
                    args["direct_internet"].as_bool() == Some(true),
                    tainted,
                    schedule.is_some(),
                    auto_approve,
                    resumed,
                    cancel,
                )
                .await
            }
            "request_secret" if self.cfg.shell_tool => {
                self.request_secret(task_id, args, tainted, resumed, schedule.is_some())
            }
            "request_access" if self.cfg.shell_tool => {
                self.request_access(task_id, args, tainted, resumed, schedule.is_some())
            }
            _ => Ok(format!("error: unknown tool {name}")),
        }
    }

    /// Handles the exact commands typed by the authenticated chat caller.
    fn apply_memory_command(
        &self,
        task_id: &str,
        command: MemoryCommand<'_>,
        login: Option<&str>,
    ) -> Result<String> {
        let actor = memory::Actor::User { login };
        match command {
            MemoryCommand::Remember { kind, text } => {
                let saved = memory::remember(
                    &self.store,
                    &self.secrets,
                    actor,
                    memory::New {
                        kind,
                        text,
                        replaces: None,
                        valid_until: None,
                    },
                )?;
                self.announce_saved(&saved, &actor.by(), Some(task_id));
                Ok(match saved {
                    memory::Saved::Active { item, .. } => format!("Remembered as a {kind} (#{}).", item.id),
                    memory::Saved::Refused(why) => format!("Memory could not be saved: {why}."),
                })
            }
            MemoryCommand::Forget(typed) => {
                // Too large to be an id: there's no such memory.
                let Ok(id) = typed.parse::<i64>() else {
                    return Ok(format!("No memory #{typed}."));
                };
                if matches!(
                    memory::forget(&self.store, actor, id)?,
                    memory::Forgot::Forgotten(_)
                ) {
                    self.emit(
                        "memory.forgotten",
                        json!({"task": task_id, "id": id, "by": actor.by()}),
                    );
                    Ok(format!("Forgot memory #{id}."))
                } else {
                    Ok(format!("No memory #{id}."))
                }
            }
        }
    }

    /// Model calls can record observations. User-stated provenance requires a
    /// direct user action, checked separately from this dispatch.
    fn memory_tool(&self, task_id: &str, name: &str, args: &Value, schedule: Option<i64>) -> Result<String> {
        let text = |key: &str| args[key].as_str().map(str::trim).filter(|v| !v.is_empty());
        let number = |key: &str| {
            args[key]
                .as_i64()
                .or_else(|| args[key].as_str()?.trim().trim_start_matches('#').parse().ok())
        };
        let actor = memory::Actor::Agent {
            run: task_id,
            seq: self
                .step_seqs
                .lock()
                .unwrap()
                .get(task_id)
                .copied()
                .filter(|s| *s > 0),
            schedule,
        };
        match name {
            "remember" | "forget" if schedule.is_some() => Ok(
                "not changed: scheduled runs don't change memories. Your report is \
                 kept with the schedule, and its next run sees it."
                    .into(),
            ),
            "remember" => {
                let Some(note) = text("text") else {
                    return Ok("error: text is required".into());
                };
                let kind = text("kind").unwrap_or("fact");
                let valid_until = match text("valid_until") {
                    None => None,
                    Some(date) => match self.end_of_day(date) {
                        Some(until) => Some(until),
                        None => return Ok("not saved: valid_until must be a date, YYYY-MM-DD".into()),
                    },
                };
                let saved = memory::remember(
                    &self.store,
                    &self.secrets,
                    actor,
                    memory::New {
                        text: note,
                        kind,
                        replaces: number("replaces"),
                        valid_until,
                    },
                )?;
                self.announce_saved(&saved, &actor.by(), Some(task_id));
                Ok(match saved {
                    memory::Saved::Active {
                        item,
                        superseded: Some(old),
                    } => {
                        format!("remembered (#{}), replacing #{old}", item.id)
                    }
                    memory::Saved::Active { item, .. } if item.kind != kind => format!(
                        "remembered as a hint (#{}). To save a standing {kind}, the user can edit it in Memory \
                         or enter the chat command \"remember {kind}: ...\"",
                        item.id
                    ),
                    memory::Saved::Active { item, .. } => {
                        format!("remembered as an inferred hint (#{})", item.id)
                    }
                    memory::Saved::Refused(why) => format!("not saved: {why}"),
                })
            }
            "forget" => {
                if let Some(id) = number("id") {
                    return Ok(match memory::forget(&self.store, actor, id)? {
                        memory::Forgot::Forgotten(_) => {
                            self.emit(
                                "memory.forgotten",
                                json!({"task": task_id, "id": id, "by": "agent"}),
                            );
                            format!("forgot #{id}")
                        }
                        memory::Forgot::Protected(_) => format!(
                            "memory #{id} requires a direct user action. Tell the user they can remove it in Memory \
                             or enter \"forget #{id}\" in chat"
                        ),
                        _ => format!("there's no memory #{id}"),
                    });
                }
                let Some(query) = text("query") else {
                    return Ok("error: give the id of the memory, or a query describing it".into());
                };
                Ok(match memory::forget_matching(&self.store, actor, query)? {
                    memory::Forgot::Forgotten(item) => {
                        self.emit(
                            "memory.forgotten",
                            json!({"task": task_id, "id": item.id, "by": "agent"}),
                        );
                        format!("forgot #{}", item.id)
                    }
                    memory::Forgot::Candidates(items) => format!(
                        "several memories match, so nothing was forgotten. Call forget with the id of the right \
                         one:\n{}",
                        items
                            .iter()
                            .map(|m| format!("#{} [{}] {}", m.id, m.kind, m.text))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                    memory::Forgot::RuleNeedsId(item) | memory::Forgot::Protected(item) => format!(
                        "memory #{} requires a direct user action. Tell the user they can remove it in Memory \
                         or enter \"forget #{}\" in chat",
                        item.id, item.id
                    ),
                    memory::Forgot::Nothing => "no memory matches that".into(),
                })
            }
            _ => {
                let Some(query) = text("query") else {
                    return Ok("error: query is required".into());
                };
                let kinds: Vec<&str> = args["kinds"]
                    .as_array()
                    .map(|k| k.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let limit = args["limit"].as_i64().unwrap_or(8).clamp(1, 20);
                let found = memory::search(&self.store, query, &kinds, limit)?;
                if found.is_empty() {
                    return Ok("nothing in memory matches that".into());
                }
                Ok(found
                    .iter()
                    .map(|(m, _)| {
                        let source = match (&m.source_schedule, &m.source_run) {
                            (Some(schedule), _) => format!("schedule #{schedule}"),
                            (None, Some(run)) => format!("task {run}"),
                            _ => "the app".into(),
                        };
                        format!(
                            "#{} [{} · {} · {} · {}] {}",
                            m.id,
                            m.kind,
                            m.date(),
                            m.trust,
                            source,
                            json!(m.text)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
        }
    }

    /// The task changed something: only those get a work-log entry when
    /// memory is tidied up (consolidate.rs).
    fn acted(&self, task_id: &str) -> Result<()> {
        self.store
            .mark_run_acted(task_id)
            .context("recording action dispatch")
    }

    /// A task's notes (memory.rs `recall`); the journal gets the ids and the
    /// size, never the text.
    fn recall(&self, task: &str, request: &str, schedule: Option<i64>) -> memory::Recall {
        let started = Instant::now();
        match memory::recall(&self.store, request, schedule) {
            Ok(recall) => {
                tracing::info!(
                    task, recalled = ?recall.recalled, tokens = recall.tokens,
                    ms = started.elapsed().as_millis() as u64, "memory block"
                );
                recall
            }
            Err(err) => {
                tracing::error!(%err, task, "couldn't build the memory block");
                memory::Recall::default()
            }
        }
    }

    /// The end of a local date (`YYYY-MM-DD`) in the user's time zone, as
    /// unix seconds.
    pub fn end_of_day(&self, date: &str) -> Option<i64> {
        end_of_day_in(date, &self.user_timezone())
    }

    /// The user's IANA time zone (from the app). It reads settings: never
    /// call it inside a `Store::transact` or `Store::read`, whose lock isn't
    /// re-entrant.
    pub fn user_timezone(&self) -> String {
        self.user_clock().timezone
    }

    /// Sets up a scheduled task. After outside content, the request may come
    /// from injected text, so the user approves it first.
    fn schedule_task(
        &self,
        task_id: &str,
        args: &Value,
        tainted: bool,
        resumed: Option<Answer>,
    ) -> Result<String> {
        let text = |key: &str| args[key].as_str().map(str::trim).filter(|v| !v.is_empty());
        let Some(prompt) = text("prompt") else {
            return Ok("error: prompt is required".into());
        };
        let prompt: String = prompt.chars().take(SCHEDULE_PROMPT_MAX_CHARS).collect();
        let name: String = text("name")
            .unwrap_or(&prompt)
            .chars()
            .take(SCHEDULE_NAME_MAX_CHARS)
            .collect();
        if self.store.schedules()?.len() >= schedule::MAX_SCHEDULES {
            return Ok(format!(
                "not scheduled: there are already {} schedules; ask the user which to cancel",
                schedule::MAX_SCHEDULES
            ));
        }
        let timezone = self.user_clock().timezone;
        let timing = match schedule::plan(text("cron"), text("at"), &timezone, jiff::Timestamp::now()) {
            Ok(timing) => timing,
            Err(problem) => return Ok(format!("not scheduled: {problem}")),
        };
        let when = schedule::describe(timing.cron.as_deref(), timing.next_run, &timezone);
        if tainted {
            let card = json!({
                "kind": "schedule", "name": name, "text": prompt, "when": when, "next_run": timing.next_run,
                "reason": "was asked for after reading outside content (a page, an email or command output)",
                "editable": true,
            });
            if !matches!(self.approve(task_id, card, resumed, false)?, Decision::Approved) {
                return Ok("denied: the user did not approve this schedule. Don't try another way.".into());
            }
        }
        let saved =
            self.store
                .add_schedule(&name, &prompt, timing.cron.as_deref(), &timezone, timing.next_run)?;
        // The rules it's set up under; "agent_tainted" when it was approved
        // through a card after outside content.
        let origin = if tainted { "agent_tainted" } else { "user" };
        self.store
            .set_schedule_origin(saved.id, &self.policy_version(), origin)?;
        // Runs without asking, as every schedule a person sets up does (§6.14),
        // unless outside content may have asked for it: then its runs skip
        // whatever would ask.
        if !tainted {
            self.store.set_schedule_auto_approve(saved.id, true)?;
        }
        // It belongs to the conversation that set it up: its runs report there.
        if let Some(session) = self.session_of(task_id) {
            self.store.set_schedule_session(saved.id, &session)?;
        }
        let saved = self.store.schedule(saved.id)?.unwrap_or(saved);
        self.schedules_changed.notify_one();
        let mut event = schedule::json(&saved);
        event["task"] = json!(task_id);
        self.emit("schedule.created", event);
        tracing::info!(task = %task_id, schedule = saved.id, "schedule created");
        Ok(format!(
            "scheduled #{} “{name}”: {when}; first run {}. Each run is a new task with just this prompt \
             (not this conversation). The user can see and cancel it in the app.",
            saved.id,
            schedule::local(timing.next_run, &timezone),
        ))
    }

    /// "Run without asking" for a schedule, set by a person in the web app (the
    /// agent has no way to): its runs then go ahead with what would ask,
    /// except root commands and browser changes. None: no such schedule.
    pub fn set_schedule_auto_approve(&self, id: i64, on: bool, by: Option<String>) -> Result<Option<Value>> {
        if !self.store.set_schedule_auto_approve(id, on)? {
            return Ok(None);
        }
        tracing::info!(schedule = id, on, "run without asking changed");
        let mut event = self
            .store
            .schedule(id)?
            .as_ref()
            .map_or_else(|| json!({"id": id}), schedule::json);
        event["by"] = json!(by);
        self.emit("schedule.updated", event.clone());
        Ok(Some(event))
    }

    /// A repeating schedule a person sets up in the web app: the same limits
    /// as schedule_task, no card (they chose it). The app sends its time zone,
    /// remembered as a message's is.
    pub fn add_user_schedule(
        &self,
        name: &str,
        prompt: &str,
        cron: &str,
        clock: Option<&UserClock>,
        by: Option<String>,
        auto_approve: bool,
    ) -> Result<Value, String> {
        let (name, prompt) = (name.trim(), prompt.trim());
        if name.is_empty() || prompt.is_empty() || cron.trim().is_empty() {
            return Err("a schedule needs a name, a prompt and when it runs".into());
        }
        let prompt: String = prompt.chars().take(SCHEDULE_PROMPT_MAX_CHARS).collect();
        let name: String = name.chars().take(SCHEDULE_NAME_MAX_CHARS).collect();
        let existing = self.store.schedules().map_err(|e| e.to_string())?;
        if existing.len() >= schedule::MAX_SCHEDULES {
            return Err(format!(
                "there are already {} schedules; cancel one first",
                schedule::MAX_SCHEDULES
            ));
        }
        if existing.iter().any(|s| s.name == name) {
            return Err(format!("there's already a schedule named “{name}”"));
        }
        if let Some(clock) = clock {
            self.set_user_clock(clock);
        }
        let timezone = self.user_clock().timezone;
        let timing = schedule::plan(Some(cron.trim()), None, &timezone, jiff::Timestamp::now())?;
        let saved = self
            .store
            .add_schedule(&name, &prompt, timing.cron.as_deref(), &timezone, timing.next_run)
            .map_err(|e| e.to_string())?;
        self.store
            .set_schedule_origin(saved.id, &self.policy_version(), "user")
            .map_err(|e| e.to_string())?;
        if auto_approve {
            self.store
                .set_schedule_auto_approve(saved.id, true)
                .map_err(|e| e.to_string())?;
            tracing::info!(schedule = saved.id, "set up to run without asking");
        }
        let saved = self
            .store
            .schedule(saved.id)
            .map_err(|e| e.to_string())?
            .unwrap_or(saved);
        self.schedules_changed.notify_one();
        let mut event = schedule::json(&saved);
        event["by"] = json!(by);
        self.emit("schedule.created", event.clone());
        tracing::info!(schedule = saved.id, by = ?by, "schedule created in the app");
        Ok(event)
    }

    /// Asks the user for a key or token: the task parks while they paste it
    /// into the app (or decline), and resumes knowing only its name.
    fn request_secret(
        &self,
        task_id: &str,
        args: &Value,
        tainted: bool,
        resumed: Option<Answer>,
        unattended: bool,
    ) -> Result<String> {
        let text = |key: &str, max: usize| -> String {
            args[key]
                .as_str()
                .map(str::trim)
                .unwrap_or_default()
                .chars()
                .take(max)
                .collect()
        };
        let name = text("name", 64);
        if let Err(problem) = secrets::check_name(&name) {
            return Ok(format!("not asked: {problem}"));
        }
        let label = Some(text("label", SECRET_LABEL_MAX_CHARS))
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| name.clone());
        let url = text("url", 2000);
        // The card shows the link's site, so a look-alike stands out.
        let host = match url::Url::parse(&url) {
            _ if url.is_empty() => None,
            Ok(link) if link.scheme() == "https" && link.host_str().is_some() => {
                link.host_str().map(String::from)
            }
            _ => {
                return Ok(
                    "not asked: url must be an https link to the page where the user creates the key".into(),
                );
            }
        };
        let replace = args["replace"].as_bool().unwrap_or(false);
        if resumed.is_none() {
            if self.secrets.has(&name) && !replace {
                return Ok(format!(
                    "{name} is already saved: use it in run_shell as ${name}. If it was rejected or has expired, \
                     ask again with replace set."
                ));
            }
            if !self.secrets.has(&name) && self.secrets.names().len() >= secrets::MAX_SECRETS {
                return Ok(
                    "not asked: there are too many saved keys; ask the user to delete some in Settings"
                        .into(),
                );
            }
        }
        let card = json!({
            "kind": "secret", "name": name, "label": label, "why": text("why", 400), "how": text("how", 3000),
            "url": (!url.is_empty()).then_some(&url), "host": host, "replace": replace, "tainted": tainted,
        });
        Ok(match self.approve(task_id, card, resumed, unattended)? {
            Decision::Unattended => format!(
                "not asked: a scheduled run can't ask for keys. Say in your report that {label} is missing; the \
                 user adds it in the web app's Keys and tokens."
            ),
            Decision::Approved if self.secrets.has(&name) => format!(
                "saved: the user gave their {label} as {name}. Commands get it as an environment variable when \
                 they name it (${name}) or list it in secrets. You can't see its value; output shows it as \
                 [secret {name}]. Pass it per command rather than writing it into files."
            ),
            Decision::Approved => {
                format!("{name} wasn't saved. Tell the user what you couldn't do without it.")
            }
            _ => format!(
                "the user didn't give {name}. Don't ask for it in the chat; tell them what you couldn't do \
                 without it."
            ),
        })
    }

    /// `request_access`: a card asking the user to let the shell reach sites
    /// the proxy refuses (access.rs). Approved, they're open at once.
    fn request_access(
        &self,
        task_id: &str,
        args: &Value,
        tainted: bool,
        resumed: Option<Answer>,
        unattended: bool,
    ) -> Result<String> {
        if self.cfg.egress_user_hosts.is_empty() {
            return Ok(
                "not asked: on this desktop, the sites the shell can reach are set by whoever deploys \
                       it. Tell the user which host you need."
                    .into(),
            );
        }
        let given: Vec<String> = match &args["hosts"] {
            Value::Array(list) => list.iter().filter_map(Value::as_str).map(String::from).collect(),
            Value::String(one) => vec![one.clone()],
            _ => Vec::new(),
        };
        if given.is_empty() {
            return Ok("not asked: give the hosts, e.g. [\"api.example.com\"]".into());
        }
        if given.len() > access::MAX_PER_REQUEST {
            return Ok(format!(
                "not asked: at most {} hosts at a time; ask only for what this task needs",
                access::MAX_PER_REQUEST
            ));
        }
        let mut hosts: Vec<String> = Vec::new();
        for raw in &given {
            match access::check_host(raw) {
                Ok(host) if !hosts.contains(&host) => hosts.push(host),
                Ok(_) => {}
                Err(problem) => return Ok(format!("not asked: {problem}")),
            }
        }
        let open = self.reachable_hosts();
        let (already, hosts): (Vec<String>, Vec<String>) =
            hosts.into_iter().partition(|h| access::covered(h, &open));
        if hosts.is_empty() {
            return Ok(format!(
                "already allowed: {}. If a command still fails, the proxy isn't what stops it.",
                already.join(", ")
            ));
        }
        if self.store.egress_hosts()?.len() + hosts.len() > access::MAX_HOSTS {
            return Ok(
                "not asked: the user has allowed too many sites; ask them to remove some in Settings".into(),
            );
        }
        let why: String = args["why"]
            .as_str()
            .unwrap_or("")
            .trim()
            .chars()
            .take(300)
            .collect();
        let list = hosts.join(", ");
        // `summary`, `reason` and `command`: what an app from before 0.15.10
        // shows on a card it doesn't know.
        let card = json!({
            "kind": "access", "hosts": hosts, "why": why, "tainted": tainted,
            "summary": format!("Let your agent reach {list}"),
            "reason": "lets its commands reach a site the desktop's proxy refuses", "command": list,
        });
        let by = resumed.as_ref().and_then(|a| a.by.clone());
        Ok(match self.approve(task_id, card, resumed, unattended)? {
            Decision::Unattended => format!(
                "not asked: a scheduled run can't ask to reach new sites. Say in your report that it needs {list}; \
                 the user can allow it in the web app's Settings."
            ),
            Decision::Approved => {
                self.acted(task_id)?;
                match self.allow_hosts(&hosts, by.as_deref(), Some(task_id), Some(&why)) {
                    Ok(_) => format!("allowed: the shell can reach {list} now. Try again."),
                    Err(err) => {
                        tracing::error!(err = %format!("{err:#}"), "couldn't open sites the user allowed");
                        format!(
                            "the user allowed {list}, but agentd couldn't open them ({err:#}). Tell the user."
                        )
                    }
                }
            }
            Decision::Changed | Decision::Denied => format!(
                "the user didn't allow {list}. Don't try another way to reach them; tell the user what you \
                 couldn't do without them."
            ),
        })
    }

    /// Everything Squid lets the shell reach: the deployment's list and the
    /// user's.
    fn reachable_hosts(&self) -> Vec<String> {
        let mut hosts = self.cfg.egress_base_hosts.clone();
        hosts.extend(
            self.store
                .egress_hosts()
                .unwrap_or_default()
                .into_iter()
                .map(|h| h.host),
        );
        hosts
    }

    /// The sites for the app: whether the user can add any here, the
    /// deployment's list, and theirs.
    pub fn access_list(&self) -> Value {
        json!({
            "enabled": !self.cfg.egress_user_hosts.is_empty(),
            "base": self.cfg.egress_base_hosts,
            "allowed": self.store.egress_hosts().unwrap_or_default(),
        })
    }

    /// Lets the shell reach `hosts` (checked already): saved, written for
    /// Squid and reloaded. Undone if Squid can't be told. How many were new.
    pub fn allow_hosts(
        &self,
        hosts: &[String],
        by: Option<&str>,
        task: Option<&str>,
        why: Option<&str>,
    ) -> Result<usize> {
        let _sync = self.access_sync.lock().unwrap();
        anyhow::ensure!(
            !self.cfg.egress_user_hosts.is_empty(),
            "this desktop's sites are set by its deployment"
        );
        let before: Vec<String> = self.store.egress_hosts()?.into_iter().map(|h| h.host).collect();
        let added = self.store.allow_hosts(hosts, by, task, why)?;
        if let Err(err) = self.sync_access() {
            for host in hosts.iter().filter(|h| !before.contains(h)) {
                let _ = self.store.remove_host(host);
            }
            let _ = self.sync_access();
            return Err(err);
        }
        tracing::info!(hosts = hosts.len(), added, "sites allowed");
        self.emit("access.allowed", json!({"hosts": hosts, "by": by, "task": task}));
        Ok(added)
    }

    /// The shell can no longer reach `host`. False if it wasn't allowed.
    pub fn remove_host(&self, host: &str, by: Option<&str>) -> Result<bool> {
        let _sync = self.access_sync.lock().unwrap();
        let removed = self.store.remove_host(host)?;
        // Reconcile even if a prior request removed the row before proxy restart.
        self.sync_access()?;
        if removed {
            self.emit("access.removed", json!({"host": host, "by": by}));
        }
        Ok(removed)
    }

    /// Writes the user's sites for Squid and reloads it if they changed.
    fn sync_access(&self) -> Result<()> {
        if self.cfg.egress_user_hosts.is_empty() {
            return Ok(());
        }
        let hosts: Vec<String> = self.store.egress_hosts()?.into_iter().map(|h| h.host).collect();
        if self.cfg.shell_proxy.is_empty() {
            access::write_list(Path::new(&self.cfg.egress_user_hosts), &hosts)?;
        } else {
            access::apply_proxy(&hosts)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_shell(
        &self,
        task_id: &str,
        command: &str,
        summary: Option<&str>,
        asked: &[String],
        timeout: u64,
        // Outside the proxy, for this command only: the user always decides.
        direct_internet: bool,
        tainted: bool,
        // A scheduled run: what would ask is refused instead, or with "Run
        // without asking" (`auto_approve`) permits only eligible worker actions.
        unattended: bool,
        auto_approve: bool,
        resumed: Option<Answer>,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<String> {
        if split_sudo(command).0 {
            return Ok("not run: system administration is available to the owner through SSM.".into());
        }
        let (keys, unknown) = self.secrets.used_by(command, asked);
        if !unknown.is_empty() {
            let saved = self.secrets.names();
            return Ok(format!(
                "not run: no saved key named {}. Saved keys: {}. Use request_secret to ask the user for one.",
                unknown.join(", "),
                if saved.is_empty() {
                    "none".into()
                } else {
                    saved.join(", ")
                },
            ));
        }
        if direct_internet && !keys.is_empty() {
            return Ok("not run: commands using saved keys must use the egress proxy.".into());
        }
        let danger = self.policy.danger(command);
        let sends_after_outside = tainted && self.policy.sends_data(command);
        let requires_review = direct_internet || !keys.is_empty() || sends_after_outside;
        let mut reason = if direct_internet {
            Some(danger.map_or(DIRECT_REASON.into(), |danger| {
                format!("{DIRECT_REASON}, and {danger}")
            }))
        } else if !keys.is_empty() {
            Some(danger.map_or("uses your saved key".into(), |danger| {
                format!("uses your saved key, and {danger}")
            }))
        } else if sends_after_outside {
            Some("could send data off the desktop after reading outside content".into())
        } else {
            danger.map(String::from)
        };
        if resumed.is_none()
            && !requires_review
            && (self.settings().auto_approve || (unattended && auto_approve))
            && let Some(why) = reason.take()
        {
            self.auto_approved(
                task_id,
                json!({"kind": "shell", "command": command, "reason": why, "tainted": tainted}),
            );
        }
        if let (Some(why), true, None) = (reason.as_deref(), unattended, &resumed) {
            tracing::info!(task = %task_id, "not run: a scheduled run's command that would ask");
            return Ok(format!(
                "not run: it {why}, and a scheduled run can't wait for the user's OK. Skip it, or say in your report what you'd run."
            ));
        }
        // The user decides on the model's plain words, so a card isn't shown
        // without them: the model is asked again (nothing is logged or shown).
        if reason.is_some() && resumed.is_none() && !guard::is_plain_summary(summary, command) {
            return Ok(guard::SUMMARY_REFUSAL.to_string());
        }
        // A resumed command's terminal card is already showing, waiting on the approval.
        if resumed.is_none() {
            self.emit(
                "shell",
                json!({
                    "task": task_id, "command": command, "as_root": false, "direct": direct_internet,
                    "needs_approval": reason.is_some(), "secrets": keys,
                }),
            );
        }
        if let Some(reason) = reason {
            // The model's plain words lead the card; the command is a tap away. After
            // outside content they may be steered, so the card says so (`tainted`).
            let mut card = json!({
                "kind": "shell", "command": command, "reason": reason, "secrets": keys,
                "summary": summary.and_then(clip_summary), "tainted": tainted, "editable": true,
            });
            card["manual"] = json!(requires_review);
            card["timeout"] = json!(timeout);
            if direct_internet {
                card["direct"] = json!(true);
            }
            if !matches!(
                self.approve(task_id, card, resumed, unattended)?,
                Decision::Approved
            ) {
                return Ok(format!(
                    "denied: the user did not approve this command (it {reason})"
                ));
            }
        }
        let started = std::time::Instant::now();
        let files: Vec<KeyFile> = keys
            .iter()
            .map(|name| KeyFile {
                name: name.clone(),
                path: self.secrets.path(name),
            })
            .collect();
        // Ordinary commands of different conversations and scheduled runs
        // run side by side: each has its own unit and pipes. A direct one
        // opens the guard firewall to the shell user, so it runs alone
        // (shell_turn). Waiting for a turn can be stopped.
        let (mut alone, mut shared) = (None, None);
        tokio::select! {
            turn = self.shell_turn.write(), if direct_internet => alone = Some(turn),
            turn = self.shell_turn.read(), if !direct_internet => shared = Some(turn),
            _ = cancel.wait_for(|stopped| *stopped) => return Ok("not run: stopped".into()),
        }
        let _turn = (alone, shared);
        let route = if direct_internet {
            Route::Direct
        } else {
            Route::Proxy
        };
        self.acted(task_id)?;
        // Every saved value is redacted before the output is cut to size
        // (shell.rs), so no part of one is left at a cut.
        let mut output = self
            .shell
            .run(command, timeout, &files, &self.secrets.snapshot(), route, cancel)
            .await?;
        if !keys.is_empty() {
            let _ = self.store.secrets_used(&keys);
        }
        // Again for a key saved while the command ran.
        output.text = self.secrets.redact(&output.text).into_owned();
        let guarded = (!self.cfg.run_as.is_empty()).then_some(route);
        let proxy_port = reqwest::Url::parse(&self.cfg.shell_proxy)
            .ok()
            .and_then(|url| url.port_or_known_default())
            .unwrap_or(DEFAULT_PROXY_PORT);
        let hint = egress_hint(&output.text, output.code, guarded, unattended, proxy_port);
        // For the log: the app's chat shows only messages and the cards waiting for the user.
        self.emit(
            "shell.output",
            json!({
                "task": task_id, "command": command, "exit_code": output.code,
                "timed_out": output.timed_out, "stopped": output.stopped, "output": output.tail(4000),
                "seconds": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            }),
        );
        let mut result = output.for_model();
        if let Some(hint) = hint {
            result.push_str(hint);
        }
        Ok(result)
    }

    /// Include a screenshot before showing a new approval card.
    async fn explain_card(&self, browser: &Browser, card: &mut Value) {
        if card["action"] != "type"
            && !guard::says_why(
                card["summary"].as_str(),
                card["target"].as_str().unwrap_or_default(),
            )
        {
            return;
        }
        match browser.screenshot_jpeg().await {
            Ok(jpeg) => match self.save_image(&jpeg) {
                Ok(id) => card["image"] = json!(format!("/api/v1/images/{id}")),
                Err(err) => tracing::warn!(%err, "couldn't save a card's picture"),
            },
            Err(err) => tracing::warn!(err = %format!("{err:#}"), "couldn't take a card's picture"),
        }
    }

    /// Review the exact browser action or explain why it cannot be taken.
    fn gate_browser_action(
        &self,
        task_id: &str,
        reason: &str,
        mut card: Value,
        resumed: Option<Answer>,
        unattended: bool,
    ) -> Result<Result<(), String>> {
        card["kind"] = json!("browser");
        card["reason"] = json!(reason);
        if card["action"] != "type"
            && resumed.is_none()
            && !unattended
            && !guard::says_why(
                card["summary"].as_str(),
                card["target"].as_str().unwrap_or_default(),
            )
        {
            return Ok(Err(guard::BROWSER_SUMMARY_REFUSAL.to_string()));
        }
        Ok(match self.approve(task_id, card, resumed, unattended)? {
            Decision::Approved => Ok(()),
            Decision::Changed => Err(PAGE_CHANGED.into()),
            Decision::Unattended => Err(format!(
                "not done: it {reason}, and a scheduled run can't wait for the user's OK. Say what you would have done."
            )),
            Decision::Denied => Err(format!(
                "denied: the user did not approve this (it {reason}). Don't try another way."
            )),
        })
    }

    /// What Auto approve let through, for the event log: no card shows it, so
    /// this is the record of what went ahead without anyone seeing it first.
    fn auto_approved(&self, task_id: &str, card: Value) {
        tracing::info!(task = %task_id, kind = card["kind"].as_str().unwrap_or(""), "auto approved");
        self.emit("approval.auto", json!({"task": task_id, "card": card}));
    }

    /// Asks the user about an action. A resumed task brings the answer to the
    /// card it parked on; otherwise this returns a Park error: the task parks
    /// and the card goes to the apps.
    fn approve(
        &self,
        task_id: &str,
        mut card: Value,
        resumed: Option<Answer>,
        unattended: bool,
    ) -> Result<Decision> {
        card["task"] = json!(task_id);
        // These actions always require a person's decision.
        let manual =
            matches!(card["kind"].as_str(), Some("secret" | "browser" | "access")) || card["manual"] == true;
        if resumed.is_none() && !manual && self.settings().auto_approve {
            self.auto_approved(task_id, card);
            return Ok(Decision::Approved);
        }
        // A scheduled run never waits on a card: nobody is watching.
        if unattended && resumed.is_none() {
            tracing::info!(task = %task_id, kind = card["kind"].as_str().unwrap_or(""), "not asked: a scheduled run");
            return Ok(Decision::Unattended);
        }
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

    async fn action_needed(&self, browser: &mut Browser, task_id: &str, reason: &str) {
        let id = short_id();
        // The tab the user takes over in; a conversation's window comes to
        // the front for it.
        browser.ensure_existing().await;
        browser.bring_to_front().await;
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
        self.notify_action_needed(task_id, &id, reason);
    }

    /// The saved keys, by name, for the system prompt.
    fn keys_prompt(&self) -> String {
        self.secret_infos()
            .iter()
            .map(|info| format!("- {}: {}\n", info.name, info.label))
            .collect()
    }

    /// The saved keys (never their values), for the model and the apps.
    pub fn secret_infos(&self) -> Vec<SecretInfo> {
        let infos = self.store.secrets().unwrap_or_default();
        self.secrets
            .names()
            .into_iter()
            .map(|name| {
                infos
                    .iter()
                    .find(|i| i.name == name)
                    .cloned()
                    .unwrap_or(SecretInfo {
                        label: name.clone(),
                        name,
                        created: String::new(),
                        updated: String::new(),
                        last_used: None,
                    })
            })
            .collect()
    }

    /// Saves a key the user typed or pasted in the app. Only its name and
    /// label are logged or shown again.
    pub fn save_secret(
        &self,
        name: &str,
        value: &str,
        label: Option<&str>,
        by: Option<String>,
    ) -> Result<SecretInfo, SecretError> {
        secrets::check_name(name).map_err(SecretError::Invalid)?;
        secrets::check_value(value).map_err(SecretError::Invalid)?;
        if !self.secrets.has(name) && self.secrets.names().len() >= secrets::MAX_SECRETS {
            return Err(SecretError::Invalid(format!(
                "there are already {} saved keys; delete some first",
                secrets::MAX_SECRETS
            )));
        }
        // The label the user gave, else the one it had, else the one asked with.
        let current = self
            .secret_infos()
            .into_iter()
            .find(|i| i.name == name)
            .map(|i| i.label);
        let asked = self.store.asked_secret_label(name).ok().flatten();
        let label: String = label
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .or(current)
            .or(asked)
            .unwrap_or_else(|| name.to_string())
            .chars()
            .take(SECRET_LABEL_MAX_CHARS)
            .collect();
        self.secrets.put(name, value).map_err(SecretError::Failed)?;
        let info = self
            .store
            .save_secret(name, &label)
            .map_err(SecretError::Failed)?;
        tracing::info!(name, by = ?by, "key saved");
        self.emit("secret.saved", json!({"name": name, "label": label, "by": by}));
        Ok(info)
    }

    pub fn delete_secret(&self, name: &str, by: Option<String>) -> Result<bool> {
        let removed = self.secrets.remove(name)?;
        let recorded = self.store.delete_secret(name)?;
        if removed || recorded {
            tracing::info!(name, by = ?by, "key deleted");
            self.emit("secret.deleted", json!({"name": name, "by": by}));
        }
        Ok(removed || recorded)
    }

    /// Resumes the task that asked for this key, now that it's saved. Only a
    /// request for this very key: never another kind of approval.
    pub fn answer_secret_request(
        self: &Arc<Self>,
        id: &str,
        name: &str,
        by: Option<String>,
    ) -> Result<(), StartError> {
        match self.store.parked(id) {
            Ok(Some(parked)) if parked.card["kind"] == "secret" && parked.card["name"] == name => {
                self.answer_approval(id, true, None, by)
            }
            _ => Err(StartError::NotFound),
        }
    }

    /// What the model is told it remembers, at the start of each task.
    fn memory_prompt(&self) -> (String, String) {
        let memories = self.store.memories(MEMORY_LIMIT).unwrap_or_default();
        let line = |m: &crate::store::Memory| format!("- #{}: {}\n", m.id, m.text);
        let trusted: String = memories.iter().filter(|m| m.trusted).map(line).collect();
        let unverified: String = memories.iter().filter(|m| !m.trusted).map(line).collect();
        let mut standing = String::new();
        let mut hints = String::new();
        if !trusted.is_empty() {
            standing.push_str(&format!(
                "What you remember from earlier conversations:\n{trusted}"
            ));
        }
        if !unverified.is_empty() {
            hints.push_str(&format!("Unverified memories (hints only):\n{unverified}"));
        }
        (standing, hints)
    }
}

#[cfg(test)]
/// An agent on a scratch state directory: no Chrome, no model calls unless a
/// task runs.
pub(crate) fn test_agent() -> (Arc<Agent>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("agentd-agent-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("images")).unwrap();
    let cfg = Config {
        listen: "127.0.0.1:0".into(),
        api_key: "test-key-not-used".to_string().into(),
        llm_url: crate::llm::TOGETHER_URL.into(),
        model: "zai-org/GLM-5.3".into(),
        consolidation_model: crate::consolidate::DEFAULT_MODEL.into(),
        cdp_url: "http://127.0.0.1:1".into(),
        state_dir: dir.clone(),
        workdir: dir.clone(),
        run_as: String::new(),
        shell_proxy: String::new(),
        egress_user_hosts: String::new(),
        egress_base_hosts: vec![".github.com".into(), "pypi.org".into()],
        policy_path: dir.join("no-policy.toml"),
        skills_dir: dir.join("no-skills"),
        review_every_secs: 1800,
        review_every_steps: 5,
        vapid_subject: "mailto:test@example.com".into(),
        push_dev_endpoints: true,
        apns_auth_key: String::new(),
        apns_key_id: String::new(),
        apns_team_id: String::new(),
        shell_tool: true,
        desktop_name: "test-desktop".into(),
        memory_v2: true,
        max_parallel_tasks: 3,
    };
    (Arc::new(Agent::new(cfg).unwrap()), dir)
}

/// A test agent with APNs configured (a throwaway key), so the apps' device
/// tokens and notifications can be exercised.
#[cfg(test)]
pub(crate) fn test_agent_with_apns() -> (Arc<Agent>, std::path::PathBuf) {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
    use base64::Engine;
    let (agent, dir) = test_agent();
    let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(key.to_pkcs8v1().unwrap().as_ref())
    );
    let mut cfg = agent.cfg.clone();
    cfg.apns_auth_key = pem;
    cfg.apns_key_id = "ABC123DEFG".into();
    cfg.apns_team_id = "TEAM123456".into();
    // Rebuild the agent on the same state directory with APNs on.
    (Arc::new(Agent::new(cfg).unwrap()), dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_agent;

    /// Parks a task on a card, as `drive` would, under `policy_version`.
    fn park(agent: &Agent, task: &str, card: Value, policy_version: &str) -> String {
        agent
            .store
            .start_run(task, "do it", None, "zai-org/GLM-5.3", policy_version, None)
            .unwrap();
        let state = json!({
            "task": task, "model": "zai-org/GLM-5.3", "allow_actions": true, "messages": [], "queue": [],
            "step": 2, "tainted": true, "reminded": false, "loops": guard::LoopDetector::default(),
            "usage": Usage::default(), "seconds": 1.0,
        });
        let id = format!("card-{task}");
        agent
            .store
            .park(&id, task, &card, &state.to_string(), policy_version)
            .unwrap();
        id
    }

    fn events(agent: &Agent, kind: &str) -> Vec<Value> {
        agent
            .store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.data)
            .collect()
    }

    #[tokio::test]
    async fn a_parked_browser_change_expires_when_browser_actions_are_turned_off() {
        let (agent, dir) = test_agent();
        let card = json!({"kind": "browser", "action": "type", "target": "Message #eng", "text": "hi", "site": "app.slack.com"});
        let id = park(&agent, "t1", card, &agent.policy_version());
        agent.update_settings(None, Some(false)).unwrap();
        assert!(matches!(
            agent.answer_approval(&id, true, None, Some("me".into())),
            Err(StartError::NotFound)
        ));
        let resolved = events(&agent, "approval.resolved");
        assert_eq!(resolved[0]["reason"], "policy_changed");
        assert_eq!(resolved[0]["expired"], true);
        let finished = events(&agent, "task.finished");
        assert_eq!(finished[0]["status"], "cancelled");
        assert!(
            events(&agent, "message")
                .iter()
                .any(|m| m["text"] == POLICY_CHANGED)
        );
        assert!(agent.store.parked(&id).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_parked_command_resumes_when_the_rules_still_only_ask() {
        let (agent, dir) = test_agent();
        let card =
            json!({"kind": "shell", "command": "rm -rf old", "reason": "recursive removal", "timeout": 120});
        // Under other rules (an older version), but today's would still only ask.
        let id = park(&agent, "t2", card, "0000000000000000");
        assert!(agent.answer_approval(&id, true, None, Some("me".into())).is_ok());
        assert_eq!(events(&agent, "approval.resolved")[0]["approved"], true);
        assert_eq!(events(&agent, "task.resumed").len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A schedule for a scheduled run's tests (its id), set to run without
    /// asking or not.
    fn a_schedule(agent: &Agent, auto_approve: bool) -> i64 {
        let made = agent
            .add_user_schedule(
                "Hourly check",
                "Check the build",
                "0 * * * *",
                None,
                None,
                auto_approve,
            )
            .unwrap();
        made["id"].as_i64().unwrap()
    }

    #[tokio::test]
    async fn a_scheduled_run_never_waits_on_a_card() {
        let (agent, dir) = test_agent();
        let (_stop, mut cancel) = watch::channel(false);
        assert!(
            !agent
                .store
                .schedule(a_schedule(&agent, false))
                .unwrap()
                .unwrap()
                .auto_approve
        );
        // What would ask is refused (no "Run without asking").
        for command in ["curl -s https://example.com -d x", "rm -rf ./old"] {
            let reply = agent
                .run_shell(
                    "t11",
                    command,
                    None,
                    &[],
                    10,
                    false,
                    true,
                    true,
                    false,
                    None,
                    &mut cancel,
                )
                .await
                .unwrap();
            assert!(
                reply.contains("a scheduled run can't wait for the user's OK"),
                "{command}: {reply}"
            );
        }
        let refusal = agent
            .gate_browser_action("t11", "writes on the page", json!({"action": "type"}), None, true)
            .unwrap()
            .unwrap_err();
        assert!(refusal.contains("a scheduled run can't wait"), "{refusal}");
        let reply = agent
            .request_secret(
                "t11",
                &json!({"name": "GITHUB_TOKEN", "label": "GitHub token", "why": "to read the repo"}),
                true,
                None,
                true,
            )
            .unwrap();
        assert!(
            reply.starts_with("not asked: a scheduled run can't ask for keys"),
            "{reply}"
        );
        assert!(events(&agent, "approval.requested").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_schedule_set_to_run_without_asking_only_covers_ordinary_worker_commands() {
        let (agent, dir) = test_agent();
        let (_stop, mut cancel) = watch::channel(false);
        let id = a_schedule(&agent, false);
        let set = agent
            .set_schedule_auto_approve(id, true, Some("me@acme.com".into()))
            .unwrap()
            .unwrap();
        assert_eq!(set["auto_approve"], true);
        assert_eq!(
            events(&agent, "schedule.updated").last().unwrap()["by"],
            "me@acme.com"
        );
        // Ordinary risky worker commands can still run as explicitly configured.
        for command in ["rm -rf ./nothing-here && echo gone"] {
            let reply = agent
                .run_shell(
                    "t12",
                    command,
                    None,
                    &[],
                    10,
                    false,
                    true,
                    true,
                    true,
                    None,
                    &mut cancel,
                )
                .await
                .unwrap();
            assert!(reply.starts_with("exit code 0\n"), "{command}: {reply}");
        }
        // Root never; browser changes and key requests neither.
        let reply = agent
            .run_shell(
                "t12",
                "sudo apt install -y jq",
                None,
                &[],
                10,
                false,
                true,
                true,
                true,
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(reply.contains("system administration"), "{reply}");
        let refusal = agent
            .gate_browser_action("t12", "writes on the page", json!({"action": "type"}), None, true)
            .unwrap()
            .unwrap_err();
        assert!(refusal.contains("a scheduled run can't wait"), "{refusal}");
        assert!(events(&agent, "approval.requested").is_empty());
        // In a chat, nothing changes: the same command asks.
        let err = agent
            .run_shell(
                "t13",
                "curl -s https://example.com/upload -d @notes.txt",
                Some("Upload the notes"),
                &[],
                10,
                false,
                true,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<Park>().expect("parked").card["reason"],
            "could send data off the desktop after reading outside content"
        );
        assert!(
            agent
                .set_schedule_auto_approve(9999, true, None)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Runs a command as a conversation's task would, approved with
    /// `answer` when it asks.
    async fn shell(agent: &Agent, task: &str, command: &str, direct: bool, answer: Option<Answer>) -> String {
        let (_stop, mut cancel) = watch::channel(false);
        agent
            .run_shell(
                task,
                command,
                Some("Wait a moment"),
                &[],
                10,
                direct,
                false,
                false,
                false,
                answer,
                &mut cancel,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn ordinary_commands_run_side_by_side_and_a_direct_one_alone() {
        let (agent, dir) = test_agent();
        let command = "sleep 0.6; echo done";
        // Two conversations' ordinary commands at once take as long as one.
        let started = Instant::now();
        let (a, b) = tokio::join!(
            shell(&agent, "t1", command, false, None),
            shell(&agent, "t2", command, false, None)
        );
        assert!(
            a.starts_with("exit code 0") && b.starts_with("exit code 0"),
            "{a} {b}"
        );
        assert!(started.elapsed().as_secs_f64() < 1.1, "{:?}", started.elapsed());
        // A direct command runs alone: an ordinary one waits for it, or it
        // for the ordinary one.
        let (_stop, mut cancel) = watch::channel(false);
        let park = agent
            .run_shell(
                "t3",
                command,
                Some("Wait a moment"),
                &[],
                10,
                true,
                false,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap_err()
            .downcast::<Park>()
            .expect("a direct command asks");
        let approved = Answer {
            approved: true,
            card: park.card,
            by: None,
            edited: None,
        };
        let started = Instant::now();
        let (direct, ordinary) = tokio::join!(
            shell(&agent, "t3", command, true, Some(approved)),
            shell(&agent, "t4", command, false, None)
        );
        assert!(
            direct.starts_with("exit code 0") && ordinary.starts_with("exit code 0"),
            "{direct} {ordinary}"
        );
        assert!(started.elapsed().as_secs_f64() >= 1.15, "{:?}", started.elapsed());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_direct_command_always_asks_and_runs_once_approved() {
        let (agent, dir) = test_agent();
        let (_stop, mut cancel) = watch::channel(false);
        // Harmless, so it wouldn't ask if it went through the proxy.
        let command = "echo direct";
        let summary = Some("Say hello");
        let err = agent
            .run_shell(
                "t20",
                command,
                summary,
                &[],
                10,
                true,
                false,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap_err();
        let park = err.downcast::<Park>().expect("asks first");
        assert_eq!(park.card["direct"], true);
        assert_eq!(park.card["reason"], DIRECT_REASON);
        assert_eq!(events(&agent, "shell")[0]["direct"], true);
        let answered = |approved: bool, card: &Value| {
            Some(Answer {
                approved,
                card: card.clone(),
                by: None,
                edited: None,
            })
        };

        let reply = agent
            .run_shell(
                "t20",
                command,
                summary,
                &[],
                10,
                true,
                false,
                false,
                false,
                answered(true, &park.card),
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(reply.starts_with("exit code 0\ndirect"), "{reply}");

        let reply = agent
            .run_shell(
                "t20",
                command,
                summary,
                &[],
                10,
                true,
                false,
                false,
                false,
                answered(false, &park.card),
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(reply.starts_with("denied:"), "{reply}");

        // A yes to the same command without direct internet is no yes to it.
        let mut proxied = park.card.clone();
        proxied.as_object_mut().unwrap().remove("direct");
        assert!(!same_request(&proxied, &park.card));
        let reply = agent
            .run_shell(
                "t20",
                command,
                summary,
                &[],
                10,
                true,
                false,
                false,
                false,
                answered(true, &proxied),
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(reply.starts_with("denied:"), "{reply}");

        // No plain words for the card: asked again, like any command that asks.
        let reply = agent
            .run_shell(
                "t20",
                command,
                None,
                &[],
                10,
                true,
                false,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(
            reply.starts_with("not run: this command needs the user's OK"),
            "{reply}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn auto_approve_cannot_bypass_protected_actions() {
        let (agent, dir) = test_agent();
        let (_stop, mut cancel) = watch::channel(false);
        agent
            .secrets
            .put("GITHUB_TOKEN", "ghp_test_value_123456")
            .unwrap();
        agent
            .set_auto_approve(true, Some(&agent.settings().auto_approve_version), None)
            .unwrap();

        let ordinary = agent
            .run_shell(
                "ordinary",
                "rm -rf ./old",
                None,
                &[],
                10,
                false,
                false,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(ordinary.starts_with("exit code 0"));
        for (command, direct, tainted) in [
            ("echo direct", true, false),
            ("curl --version", false, true),
            ("echo $GITHUB_TOKEN", false, false),
        ] {
            let result = agent
                .run_shell(
                    "protected",
                    command,
                    Some("Review this operation"),
                    &[],
                    10,
                    direct,
                    tainted,
                    false,
                    false,
                    None,
                    &mut cancel,
                )
                .await;
            assert!(result.unwrap_err().downcast::<Park>().is_ok(), "{command}");
            let scheduled = agent
                .run_shell(
                    "scheduled",
                    command,
                    None,
                    &[],
                    10,
                    direct,
                    tainted,
                    true,
                    true,
                    None,
                    &mut cancel,
                )
                .await
                .unwrap();
            assert!(
                scheduled.contains("scheduled run can't wait"),
                "{command}: {scheduled}"
            );
        }
        let root = agent
            .run_shell(
                "root",
                "sudo true",
                None,
                &[],
                10,
                false,
                false,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(root.contains("system administration"));
        assert!(
            agent
                .gate_browser_action(
                    "browser",
                    "writes on the page",
                    json!({"action": "type"}),
                    None,
                    false
                )
                .unwrap_err()
                .downcast::<Park>()
                .is_ok()
        );
        assert_eq!(events(&agent, "approval.auto").len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn read_only_and_scheduled_tasks_block_browser_input_before_contacting_chrome() {
        let (agent, dir) = test_agent();
        let mut browser = Browser::new(agent.cdp.clone());
        let (_stop, mut cancel) = watch::channel(false);
        agent.update_settings(None, Some(false)).unwrap();
        let args = json!({"role": "link", "name": "Not assumed safe"});
        let blocked = agent
            .call_tool(
                &mut browser,
                "read-only",
                "click",
                &args,
                false,
                None,
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(blocked.contains("browser inputs are disabled"));
        agent.update_settings(None, Some(true)).unwrap();
        let scheduled = agent
            .call_tool(
                &mut browser,
                "scheduled",
                "click",
                &args,
                false,
                Some(1),
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(scheduled.contains("require approval"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The state a task parks in, with `args` as the call it parked on.
    fn parked_on(tool: &str, args: &Value) -> RunState {
        serde_json::from_value(json!({
            "task": "t1", "model": "m", "allow_actions": true, "messages": [], "step": 1,
            "tainted": true, "reminded": false, "loops": guard::LoopDetector::default(),
            "usage": Usage::default(), "seconds": 0.0,
            "queue": [{"id": "c1", "function": {"name": tool, "arguments": args.to_string()}}],
        }))
        .unwrap()
    }

    fn call_args(state: &RunState) -> Value {
        serde_json::from_str(state.queue[0]["function"]["arguments"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn an_edited_command_is_what_runs_and_counts_as_approved() {
        let (agent, dir) = test_agent();
        agent
            .secrets
            .put("GITHUB_TOKEN", "ghp_test_value_123456")
            .unwrap();
        let (_stop, mut cancel) = watch::channel(false);
        let original = "rm -rf ./old";
        let park = agent
            .run_shell(
                "t1",
                original,
                Some("Clear the old build"),
                &[],
                10,
                false,
                false,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap_err()
            .downcast::<Park>()
            .unwrap();
        assert_eq!(park.card["editable"], true);
        let mut state = parked_on(
            "run_shell",
            &json!({"command": original, "summary": "Clear the old build"}),
        );
        let mut card = park.card;
        // The edit is what runs, with the same authorization as the original.
        let edited = "rm -rf ./older; echo updated";
        assert_eq!(
            agent.apply_edit(&mut state, &mut card, edited).as_deref(),
            Some(edited)
        );
        let args = call_args(&state);
        assert_eq!(args["command"], edited);
        assert_eq!(
            args["summary"], "Clear the old build",
            "the rest of the call stays"
        );
        let reply = agent
            .run_shell(
                "t1",
                edited,
                Some("Clear the old build"),
                &[],
                10,
                false,
                false,
                false,
                false,
                Some(Answer {
                    approved: true,
                    card,
                    by: None,
                    edited: Some(edited.into()),
                }),
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(reply.starts_with("exit code 0"), "{reply}");
        assert!(reply.contains("updated"), "{reply}");
        // The model hears what ran, and never a key's value, even one typed into the edit.
        let note = agent.edited_note("echo ghp_test_value_123456");
        assert!(
            note.contains("[secret GITHUB_TOKEN]") && !note.contains("ghp_test"),
            "{note}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_edit_that_matches_nothing_does_not_run_the_agents_words() {
        let (agent, dir) = test_agent();
        let (_stop, mut cancel) = watch::channel(false);
        let park = agent
            .run_shell(
                "t1",
                "rm -rf ./old",
                Some("Clear the old build"),
                &[],
                10,
                false,
                false,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap_err()
            .downcast::<Park>()
            .unwrap();
        // Approved for one command: another one isn't covered.
        let reply = agent
            .run_shell(
                "t1",
                "rm -rf ./other",
                Some("Clear the old build"),
                &[],
                10,
                false,
                false,
                false,
                false,
                Some(Answer {
                    approved: true,
                    card: park.card,
                    by: None,
                    edited: None,
                }),
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(reply.starts_with("denied"), "{reply}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn edited_browser_text_matches_the_full_payload() {
        let (agent, dir) = test_agent();
        let mut state = parked_on("type_text", &json!({"text": "Hi there"}));
        let mut card = json!({
            "kind": "browser", "action": "type", "target": "Message", "text": "Hi there", "editable": true,
        });
        let long = "a".repeat(CARD_TEXT_MAX_CHARS + 1);
        assert!(agent.apply_edit(&mut state, &mut card, &long).is_none());
        assert_eq!(call_args(&state)["text"], "Hi there");

        let exact = format!(
            "  Hello,

{}",
            "a".repeat(CARD_TEXT_MAX_CHARS - 10)
        );
        assert!(agent.apply_edit(&mut state, &mut card, &exact).is_some());
        assert_eq!(call_args(&state)["text"], exact.as_str());
        assert_eq!(card["text"], exact.as_str());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_edited_schedule_keeps_its_name_and_uses_the_new_instructions() {
        let (agent, dir) = test_agent();
        // The model gave no name, so the card's came from its instructions.
        let args = json!({"prompt": "Check everything", "cron": "0 8 * * *"});
        let park = agent
            .schedule_task("t1", &args, true, None)
            .unwrap_err()
            .downcast::<Park>()
            .unwrap();
        assert_eq!(park.card["editable"], true);
        let mut state = parked_on("schedule_task", &args);
        let mut card = park.card;
        assert_eq!(
            agent
                .apply_edit(&mut state, &mut card, "  Check only the build \n")
                .as_deref(),
            Some("Check only the build")
        );
        let reply = agent
            .schedule_task(
                "t1",
                &call_args(&state),
                true,
                Some(Answer {
                    approved: true,
                    card,
                    by: None,
                    edited: Some("Check only the build".into()),
                }),
            )
            .unwrap();
        assert!(reply.starts_with("scheduled #"), "{reply}");
        let saved = agent.store.schedules().unwrap();
        assert_eq!(saved[0].prompt, "Check only the build");
        assert_eq!(saved[0].name, "Check everything");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_a_card_that_says_so_takes_an_edit() {
        let (agent, dir) = test_agent();
        let mut state = parked_on("click", &json!({"text": "Send"}));
        for mut card in [
            json!({"kind": "browser", "action": "click", "target": "Send", "editable": true}),
            json!({"kind": "access", "hosts": ["a.example.com"], "editable": true}),
            json!({"kind": "secret", "name": "A_KEY", "editable": true}),
            // Parked by an older agentd, which wouldn't say.
            json!({"kind": "shell", "command": "ls"}),
        ] {
            assert!(
                agent.apply_edit(&mut state, &mut card, "other").is_none(),
                "{card}"
            );
        }
        let mut card = json!({"kind": "shell", "command": "ls", "editable": true});
        let mut shell = parked_on("run_shell", &json!({"command": "ls"}));
        assert!(agent.apply_edit(&mut shell, &mut card, " \n ").is_none(), "blank");
        assert_eq!(call_args(&shell)["command"], "ls", "nothing changed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_click_that_asks_has_to_say_why() {
        let (agent, dir) = test_agent();
        let ask = |card: Value, resumed: Option<Answer>, unattended: bool| {
            agent.gate_browser_action(
                "t1",
                "may send, delete or change something",
                card,
                resumed,
                unattended,
            )
        };
        // No reason, or only its own label: the model is asked again, and nobody sees a card.
        for summary in [json!(null), json!("  "), json!("Confirm")] {
            let card = json!({"action": "click", "target": "Confirm", "summary": summary, "site": "x.com"});
            let refusal = ask(card, None, false).unwrap().unwrap_err();
            assert!(refusal.starts_with("not run:"), "{refusal}");
            assert_eq!(refusal, guard::BROWSER_SUMMARY_REFUSAL);
        }
        // A key too.
        let refusal = ask(
            json!({"action": "key", "key": "Enter", "target": ""}),
            None,
            false,
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(refusal, guard::BROWSER_SUMMARY_REFUSAL);
        // With one, it asks. Symbols are fine: pages have them.
        let card =
            json!({"action": "click", "target": "Confirm", "summary": "Pay $20 to Acme", "site": "x.com"});
        let park = ask(card.clone(), None, false)
            .unwrap_err()
            .downcast::<Park>()
            .unwrap();
        assert_eq!(park.card["summary"], "Pay $20 to Acme");
        // Typing shows its text, so it needs none.
        assert!(ask(json!({"action": "type", "text": "hi"}), None, false).is_err());
        // Nobody asked, nothing to explain: a scheduled run refuses as it always did…
        let refusal = ask(json!({"action": "click", "target": "Confirm"}), None, true)
            .unwrap()
            .unwrap_err();
        assert!(refusal.contains("a scheduled run can't wait"), "{refusal}");
        // Auto approve still requires an explicit browser review.
        agent
            .set_auto_approve(true, Some(&agent.settings().auto_approve_version), None)
            .unwrap();
        assert!(ask(card, None, false).unwrap_err().downcast::<Park>().is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn browser_approval_binds_document_destination_and_full_text() {
        let card = json!({
            "kind": "browser", "action": "type", "target": "Message", "text": "Reviewed text",
            "url": "https://example.com/inbox#one", "destination": "https://example.com/drafts",
            "binding": {"tab": "T1", "document": "D1", "node": 1, "draft": "one"}
        });
        for (field, changed) in [
            ("url", json!("https://example.com/inbox#two")),
            ("destination", json!("https://example.com/messages")),
            ("text", json!("Reviewed text with extra words")),
            (
                "binding",
                json!({"tab": "T1", "document": "D2", "node": 1, "draft": "one"}),
            ),
            (
                "binding",
                json!({"tab": "T1", "document": "D1", "node": 1, "draft": "two"}),
            ),
        ] {
            let mut current = card.clone();
            current[field] = changed;
            assert!(!same_request(&card, &current), "{field}");
        }
    }

    #[test]
    fn a_row_that_changed_is_not_the_one_approved() {
        let approved =
            json!({"kind": "browser", "action": "click", "target": "Delete", "around": "old-build Delete"});
        let same =
            json!({"kind": "browser", "action": "click", "target": "Delete", "around": "old-build Delete"});
        let other =
            json!({"kind": "browser", "action": "click", "target": "Delete", "around": "main Delete"});
        assert!(same_request(&approved, &same));
        assert!(!same_request(&approved, &other));
    }

    #[test]
    fn auto_approve_can_not_make_up_a_key() {
        let (agent, dir) = test_agent();
        agent
            .set_auto_approve(true, Some(&agent.settings().auto_approve_version), None)
            .unwrap();
        let args = json!({"name": "GITHUB_TOKEN", "why": "to clone the repo"});
        // A chat still asks the user for it, and a scheduled run says it's missing.
        let park = agent
            .request_secret("t1", &args, false, None, false)
            .unwrap_err()
            .downcast::<Park>()
            .expect("a key has to come from a person");
        assert_eq!(park.card["kind"], "secret");
        let reply = agent.request_secret("t2", &args, false, None, true).unwrap();
        assert!(reply.starts_with("not asked: a scheduled run"), "{reply}");
        assert!(events(&agent, "approval.auto").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_scheduled_run_never_connects_directly() {
        let (agent, dir) = test_agent();
        let (_stop, mut cancel) = watch::channel(false);
        // Not with "Run without asking" either.
        for auto_approve in [false, true] {
            let reply = agent
                .run_shell(
                    "t21",
                    "echo direct",
                    None,
                    &[],
                    10,
                    true,
                    false,
                    true,
                    auto_approve,
                    None,
                    &mut cancel,
                )
                .await
                .unwrap();
            assert!(reply.starts_with("not run: it connects directly"), "{reply}");
        }
        assert!(events(&agent, "approval.requested").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_direct_card_keeps_what_else_it_asks_about() {
        let (agent, dir) = test_agent();
        let (_stop, mut cancel) = watch::channel(false);
        let park = |err: anyhow::Error| err.downcast::<Park>().expect("asks");
        // A dangerous command still says so.
        let card = park(
            agent
                .run_shell(
                    "t22",
                    "rm -rf ./old",
                    Some("Clear the old build"),
                    &[],
                    10,
                    true,
                    false,
                    false,
                    false,
                    None,
                    &mut cancel,
                )
                .await
                .unwrap_err(),
        )
        .card;
        let reason = card["reason"].as_str().unwrap();
        assert!(
            reason.starts_with(DIRECT_REASON) && reason.contains(", and "),
            "{reason}"
        );
        // Ordinary proxy commands never show a direct network grant.
        let card = park(
            agent
                .run_shell(
                    "t24",
                    "rm -rf ./old",
                    Some("Clear the old build"),
                    &[],
                    10,
                    false,
                    false,
                    false,
                    false,
                    None,
                    &mut cancel,
                )
                .await
                .unwrap_err(),
        )
        .card;
        assert!(card.get("direct").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_commands_output_says_which_way_out_to_try() {
        let proxy = Some(Route::Proxy);
        let hint = |text, code, route, unattended| egress_hint(text, code, route, unattended, 3128);
        let direct =
            "transport: Error while dialing: dial tcp 149.248.212.172:443: connect: connection refused";
        assert_eq!(hint(direct, Some(1), proxy, false), Some(DIRECT_HINT));
        assert_eq!(hint(direct, Some(1), proxy, true), Some(DIRECT_HINT_SCHEDULED));
        // Timed out while it kept retrying: it failed too.
        assert_eq!(hint(direct, None, proxy, false), Some(DIRECT_HINT));
        // It worked and merely printed that (a log it read).
        assert_eq!(hint(direct, Some(0), proxy, false), None);
        // Outside the proxy's firewall rules (a direct command, local
        // development) that isn't why it failed.
        assert_eq!(hint(direct, Some(1), Some(Route::Direct), false), None);
        assert_eq!(hint(direct, Some(1), None, false), None);

        let refused = "curl: (56) CONNECT tunnel failed, response 403";
        assert_eq!(hint(refused, Some(56), proxy, false), Some(ACCESS_HINT));
        assert_eq!(hint(refused, Some(56), proxy, true), Some(ACCESS_HINT_SCHEDULED));
        assert_eq!(hint("all good", Some(0), proxy, false), None);

        // A server on this machine: the firewall, whichever way out, and
        // never request_access or direct_internet.
        let local =
            "curl: (7) Failed to connect to localhost port 3000 after 0 ms: Couldn't connect to server";
        for route in [proxy, Some(Route::Direct)] {
            assert_eq!(hint(local, Some(7), route, false), Some(LOCAL_HINT));
            assert_eq!(hint(local, Some(7), route, true), Some(LOCAL_HINT_SCHEDULED));
        }
        assert!(!LOCAL_HINT.contains("call request_access") && !LOCAL_HINT.contains("with direct_internet"));
        assert_eq!(hint(local, Some(0), proxy, false), None);
        // Local development has no firewall: a server there is just down.
        assert_eq!(hint(local, Some(7), None, false), None);
        // The proxy itself not answering isn't a local server.
        let proxy_down =
            "fatal: unable to access 'https://github.com/': Failed to connect to 127.0.0.1 port 3128";
        assert_eq!(hint(proxy_down, Some(128), proxy, false), None);
    }

    #[tokio::test]
    async fn a_command_without_plain_words_is_sent_back_not_shown() {
        let (agent, dir) = test_agent();
        let (_stop, mut cancel) = watch::channel(false);
        for summary in [None, Some("rm -rf ./old"), Some("env | grep -i proxy")] {
            let reply = agent
                .run_shell(
                    "t5",
                    "rm -rf ./old",
                    summary,
                    &[],
                    10,
                    false,
                    false,
                    false,
                    false,
                    None,
                    &mut cancel,
                )
                .await
                .unwrap();
            assert!(
                reply.starts_with("not run: this command needs the user's OK"),
                "{reply}"
            );
        }
        // Nothing reached the user: no card, not even the command.
        assert!(events(&agent, "shell").is_empty() && events(&agent, "approval.requested").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_command_card_leads_with_plain_words() {
        let (agent, dir) = test_agent();
        let (_stop, mut cancel) = watch::channel(false);
        let command = "curl --version";
        let summary = "  Check which   proxy\n the shell uses ";
        let err = agent
            .run_shell(
                "t4",
                command,
                Some(summary),
                &[],
                10,
                false,
                true,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap_err();
        let park = err.downcast_ref::<Park>().expect("parked on a card");
        assert_eq!(park.card["summary"], "Check which proxy the shell uses");
        assert_eq!(park.card["command"], command);
        assert_eq!(
            park.card["reason"],
            "could send data off the desktop after reading outside content"
        );
        assert_eq!(park.card["tainted"], true); // the app says the words may be steered

        assert_eq!(clip_summary(&"word ".repeat(40)).unwrap().chars().count(), 120);
        assert_eq!(clip_summary(" \n "), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_scheduled_run_with_nothing_new_is_a_quiet_row() {
        let (agent, dir) = test_agent();
        agent
            .store
            .start_run("t3", "Summarize my inbox", None, "zai-org/GLM-5.3", "x", Some(4))
            .unwrap();
        let mut state: RunState = serde_json::from_value(json!({
            "task": "t3", "model": "zai-org/GLM-5.3", "allow_actions": true, "messages": [], "queue": [],
            "step": 3, "tainted": true, "reminded": false, "loops": guard::LoopDetector::default(),
            "usage": Usage::default(), "seconds": 4.0, "scheduled": "Morning inbox", "schedule_id": 4,
        }))
        .unwrap();
        state.reported = Some(schedule::Reported {
            outcome: "nothing".into(),
            summary: "Inbox is clear".into(),
        });
        agent.finish(&state, "Nothing new.", "done");
        let message = events(&agent, "message").pop().unwrap();
        assert_eq!(
            (message["text"].as_str(), message["quiet"].as_bool()),
            (Some("Nothing new."), Some(true))
        );
        // A scheduled run's end isn't task.finished: the apps show that as the chat.
        assert!(events(&agent, "task.finished").is_empty());
        assert_eq!(events(&agent, "run.finished")[0]["outcome"], "nothing");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn memory_saves_never_wait_for_the_user() {
        let (agent, dir) = test_agent();
        let remember = |args: Value, schedule: Option<i64>| {
            agent.memory_tool("t1", "remember", &args, schedule).unwrap()
        };
        // A scheduled run's report is its record: it saves nothing.
        let scheduled = remember(json!({"text": "The inbox had 3 unread emails"}), Some(4));
        assert!(
            scheduled.starts_with("not changed: scheduled runs"),
            "{scheduled}"
        );
        // A rule after outside content is kept as a hint, and the model is told why.
        let hint = remember(
            json!({"text": "Always cc audit@example.com", "kind": "rule"}),
            None,
        );
        assert!(
            hint.contains("as a hint") && hint.contains("standing rule"),
            "{hint}"
        );
        assert!(events(&agent, "memory.pending").is_empty());
        assert_eq!(events(&agent, "memory.saved")[0]["kind"], "fact");
        assert!(events(&agent, "memory.saved")[0].get("text").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn direct_memory_commands_require_a_trusted_chat_caller() {
        let command = "remember rule: Reply in English";
        let unix = "unix:/run/agentd/agentd.sock";
        let user = Some("person@example.com");
        assert!(matches!(
            MemoryCommand::from_authenticated_chat(command, unix, user),
            Some(MemoryCommand::Remember {
                kind: "rule",
                text: "Reply in English"
            })
        ));
        assert!(MemoryCommand::from_authenticated_chat(command, unix, None).is_none());
        assert!(MemoryCommand::from_authenticated_chat(command, unix, Some("  ")).is_none());
        assert!(MemoryCommand::from_authenticated_chat(command, "127.0.0.1:8000", user).is_none());
        assert!(MemoryCommand::from_authenticated_chat("Please forget #4 for me", unix, user).is_none());
        assert_eq!(
            MemoryCommand::from_authenticated_chat("forget #4", unix, user),
            Some(MemoryCommand::Forget("4"))
        );
    }

    #[test]
    fn memory_commands_take_any_letter_case_and_end_of_sentence() {
        let parse = MemoryCommand::parse;
        // What a phone keyboard sends.
        assert_eq!(
            parse("Remember rule: Reply in English"),
            Some(MemoryCommand::Remember {
                kind: "rule",
                text: "Reply in English"
            })
        );
        assert_eq!(
            parse("  REMEMBER Preference:  dark mode  "),
            Some(MemoryCommand::Remember {
                kind: "preference",
                text: "dark mode"
            })
        );
        assert_eq!(
            parse("Remember fact: The office is in Lisbon."),
            Some(MemoryCommand::Remember {
                kind: "fact",
                text: "The office is in Lisbon."
            })
        );
        for forget in [
            "Forget #12",
            "forget #12.",
            "FORGET #12!  ",
            "forget #12 ?",
            "forget #12\n",
        ] {
            assert_eq!(parse(forget), Some(MemoryCommand::Forget("12")), "{forget:?}");
        }
        // Too large for an id: still the command (answered "No memory").
        assert_eq!(
            parse("forget #99999999999999999999999"),
            Some(MemoryCommand::Forget("99999999999999999999999"))
        );
        // Only at the start, and only the exact forms.
        for not_a_command in [
            "Please remember rule: be brief",
            "remember: be brief",
            "remember rules: be brief",
            "forget #",
            "forget #twelve",
            "forget #12 and #13",
            "forget #12-13",
            "forget 12",
            "Forget about #12",
            "rémember rule: x",
        ] {
            assert_eq!(parse(not_a_command), None, "{not_a_command:?}");
        }
    }

    /// An agent listening on the API socket, as on the desktop, where the
    /// signed-in chat's memory commands apply.
    fn socket_agent() -> (Arc<Agent>, std::path::PathBuf) {
        let (agent, dir) = test_agent();
        let mut cfg = agent.cfg.clone();
        cfg.listen = "unix:/run/agentd/agentd.sock".into();
        (Arc::new(Agent::new(cfg).unwrap()), dir)
    }

    async fn until_idle(agent: &Agent) {
        for _ in 0..200 {
            if !agent.is_busy() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the task didn't finish");
    }

    #[tokio::test]
    async fn a_memory_command_leaves_a_parked_card_waiting() {
        let (agent, dir) = socket_agent();
        let card =
            json!({"kind": "shell", "command": "rm -rf old", "reason": "recursive removal", "timeout": 120});
        let id = park(&agent, "t1", card, &agent.policy_version());
        let started = agent.post(
            MAIN,
            "Remember fact: The office is in Lisbon".into(),
            Some("me@example.com".into()),
        );
        assert!(matches!(started, Ok(Posted::Started(_))));
        until_idle(&agent).await;
        assert!(
            agent.store.parked(&id).unwrap().is_some(),
            "the card was given up on"
        );
        assert!(events(&agent, "approval.resolved").is_empty());
        assert_eq!(events(&agent, "memory.saved")[0]["kind"], "fact");
        let answer = events(&agent, "message")
            .into_iter()
            .rfind(|m| m["role"] == "assistant")
            .unwrap();
        assert!(
            answer["text"]
                .as_str()
                .unwrap()
                .starts_with("Remembered as a fact")
        );

        // An id too large to exist gets the usual answer, not the model.
        let started = agent.post(
            MAIN,
            "forget #99999999999999999999999.".into(),
            Some("me@example.com".into()),
        );
        assert!(matches!(started, Ok(Posted::Started(_))));
        until_idle(&agent).await;
        let answer = events(&agent, "message")
            .into_iter()
            .rfind(|m| m["role"] == "assistant")
            .unwrap();
        assert_eq!(answer["text"], "No memory #99999999999999999999999.");
        assert!(agent.store.parked(&id).unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn memory_content_is_removed_from_parked_state() {
        let mut checkpoint = json!({
            "messages": [
                {"role": "system", "content": "Old standing memory"},
                {"role": "assistant", "tool_calls": [
                    {"id": "memory", "function": {"name": "search_memory", "arguments": "old search"}},
                    {"id": "browser", "function": {"name": "open_url", "arguments": "browser args"}}
                ]},
                {"role": "tool", "tool_call_id": "memory", "content": "Old memory result"},
                {"role": "tool", "tool_call_id": "browser", "content": "Browser result"},
                {"role": "assistant", "tool_calls": [
                    {"id": "shell", "function": {"name": "run_shell", "arguments": "shell args"}},
                    {"id": "queued", "function": {"name": "remember", "arguments": "queued text"}}
                ]}
            ],
            "queue": [
                {"id": "shell", "function": {"name": "run_shell", "arguments": "shell args"}},
                {"id": "queued", "function": {"name": "remember", "arguments": "queued text"}}
            ],
            "loops": {"last": "remember earlier text", "failures": 1}
        });
        scrub_memory_checkpoint(&mut checkpoint);
        assert_eq!(checkpoint["messages"][0]["content"], "");
        assert_eq!(
            checkpoint["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
        assert_eq!(
            checkpoint["messages"][1]["tool_calls"][1]["function"]["arguments"],
            "browser args"
        );
        assert_eq!(
            checkpoint["messages"][2]["content"],
            "Memory context is refreshed after a pause."
        );
        assert_eq!(checkpoint["messages"][3]["content"], "Browser result");
        // A memory call that hasn't run yet keeps what it asks for.
        assert_eq!(checkpoint["queue"][1]["function"]["arguments"], "queued text");
        assert_eq!(
            checkpoint["messages"][4]["tool_calls"][1]["function"]["arguments"],
            "queued text"
        );
        assert!(checkpoint["loops"]["last"].is_null());
        // Scrubbing again (on load) changes nothing more.
        let once = checkpoint.clone();
        scrub_memory_checkpoint(&mut checkpoint);
        assert_eq!(checkpoint, once);
    }

    #[test]
    fn a_memory_call_queued_behind_an_approval_runs_after_it() {
        let (agent, dir) = test_agent();
        let remember = json!({"text": "The deploy key lives in the team vault", "kind": "fact"}).to_string();
        let mut state = parked_on(
            "run_shell",
            &json!({"command": "rm -rf old", "summary": "Clear it"}),
        );
        let queued =
            json!({"id": "c2", "type": "function", "function": {"name": "remember", "arguments": remember}});
        state.messages = vec![
            json!({"role": "system", "content": "standing notes"}),
            json!({"role": "assistant", "tool_calls": [state.queue[0].clone(), queued.clone()]}),
        ];
        state.queue.push(queued);
        let park = Park {
            id: "p1".into(),
            card: json!({"kind": "shell", "command": "rm -rf old", "timeout": 120}),
        };
        agent
            .store
            .start_run("t1", "do it", None, "m", &agent.policy_version(), None)
            .unwrap();
        agent.save_parked(&state, &park).unwrap();
        let checkpoint = agent.store.parked("p1").unwrap().unwrap().checkpoint.unwrap();
        let resumed = load_parked_state(&checkpoint).unwrap();
        assert_eq!(resumed.messages[0]["content"], "");
        let args: Value =
            serde_json::from_str(resumed.queue[1]["function"]["arguments"].as_str().unwrap()).unwrap();
        let reply = agent.memory_tool("t1", "remember", &args, None).unwrap();
        assert!(reply.starts_with("remembered"), "{reply}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn opening_a_page_is_not_an_action() {
        let (agent, dir) = test_agent();
        agent
            .store
            .start_run("t1", "read the news", None, "zai-org/GLM-5.3", "x", None)
            .unwrap();
        let acted = || -> Option<i64> {
            agent
                .store
                .read(|conn| Ok(conn.query_row("SELECT acted FROM runs WHERE id = 't1'", [], |r| r.get(0))?))
                .unwrap()
        };
        let mut browser = Browser::new(agent.cdp.clone());
        let (_stop, mut cancel) = watch::channel(false);
        let opened = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            agent.call_tool(
                &mut browser,
                "t1",
                "open_url",
                &json!({"url": "https://example.com/"}),
                false,
                None,
                None,
                &mut cancel,
            ),
        )
        .await
        .expect("no Chrome here: it fails at once");
        drop(opened);
        // Retried after a failure or restart, and no work-log entry (§6.13).
        assert_eq!(acted(), Some(0));
        assert!(agent.store.run_has_no_actions("t1").unwrap());
        // A command is.
        let ran = agent
            .run_shell(
                "t1",
                "echo hi",
                None,
                &[],
                10,
                false,
                false,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(ran.starts_with("exit code 0"), "{ran}");
        assert_eq!(acted(), Some(1));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_edit_with_a_saved_key_is_refused_with_a_reason() {
        let (agent, dir) = test_agent();
        agent
            .secrets
            .put("GITHUB_TOKEN", "ghp_test_value_123456")
            .unwrap();
        let card = json!({
            "kind": "browser", "action": "type", "target": "Message", "text": "Hi", "editable": true,
            "binding": {"tab": "T1", "document": "D1", "node": 1},
        });
        let id = park(&agent, "t1", card, &agent.policy_version());
        let problem = agent
            .edit_problem(&id, "Here it is: ghp_test_value_123456")
            .expect("refused before answering");
        assert!(
            problem.contains("saved key") && !problem.contains("ghp_"),
            "{problem}"
        );
        assert!(
            agent
                .edit_problem(&id, &"a".repeat(CARD_TEXT_MAX_CHARS + 1))
                .is_some_and(|p| p.contains("too long"))
        );
        assert_eq!(agent.edit_problem(&id, "Hi there"), None);
        // A command's edit may name a key (the card shows it): edits there
        // can't add one, which resuming checks.
        let shell = json!({"kind": "shell", "command": "ls", "editable": true, "timeout": 120});
        let shell_id = park(&agent, "t2", shell, &agent.policy_version());
        assert_eq!(agent.edit_problem(&shell_id, "echo ghp_test_value_123456"), None);
        assert!(agent.store.parked(&id).unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_task_is_marked_when_it_changes_something() {
        let (agent, dir) = test_agent();
        agent
            .store
            .start_run("t1", "fix the build", None, "zai-org/GLM-5.3", "x", None)
            .unwrap();
        let acted = || -> Option<i64> {
            agent
                .store
                .read(|conn| Ok(conn.query_row("SELECT acted FROM runs WHERE id = 't1'", [], |r| r.get(0))?))
                .unwrap()
        };
        assert_eq!(acted(), Some(0));
        agent.acted("t1").unwrap();
        assert_eq!(acted(), Some(1));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn spots_requests_to_remember() {
        for text in [
            "Remember that my favourite colour is teal",
            "please don't forget I'm vegetarian",
            "Dont forget the standup moved",
            "Keep in mind that Dana is on leave",
            "From now on, reply in French",
        ] {
            assert!(ASKS_TO_REMEMBER.is_match(text), "{text}");
        }
        for text in [
            "Summarize my inbox",
            "Forget it",
            "the remembrance service",
            "now on to the next",
        ] {
            assert!(!ASKS_TO_REMEMBER.is_match(text), "{text}");
        }
    }

    #[test]
    fn empty_replies_are_spotted() {
        for message in [json!({"content": null}), json!({}), json!({"content": "  \n"})] {
            assert!(is_empty_reply(&message), "{message}");
        }
        assert!(!is_empty_reply(&json!({"content": "Today: chapter 2"})));
    }

    #[test]
    fn checkpoints_from_before_memory_v2_still_load() {
        // A task parked by 0.15.0 phase 0 (no memory fields) resumes.
        let old = json!({
            "task": "t1", "model": "m", "allow_actions": true, "messages": [], "queue": [], "step": 3,
            "tainted": false, "reminded": false, "loops": guard::LoopDetector::default(),
            "usage": Usage::default(), "seconds": 1.0, "scheduled": "Morning inbox",
            // Written by agentd before the supervisor: ignored now.
            "asked_again": true,
        });
        let state: RunState = serde_json::from_value(old).unwrap();
        assert_eq!(
            (
                state.remember_asked,
                state.remembered,
                state.nudged,
                state.schedule_id,
                state.empty_retries
            ),
            (false, false, false, None, 0)
        );
        assert_eq!(state.scheduled.as_deref(), Some("Morning inbox"));
        assert!(state.plan.is_empty() && state.review.at_step == 0 && state.review.stuck.is_none());
    }

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

#[cfg(test)]
mod user_schedule_tests {
    use super::*;

    #[test]
    fn the_app_sets_up_schedules_within_the_same_limits() {
        let (agent, dir) = test_agent();
        let clock = UserClock::from_app("America/Toronto", -18000).unwrap();
        let made = agent
            .add_user_schedule(
                "Morning inbox",
                "Summarize the last 24 hours of email.",
                "0 8 * * 1-5",
                Some(&clock),
                Some("me@acme.com".into()),
                true,
            )
            .unwrap();
        assert_eq!(made["name"], "Morning inbox");
        assert_eq!(made["auto_approve"], true);
        assert_eq!(made["timezone"], "America/Toronto");
        assert_eq!(
            agent.user_clock().timezone,
            "America/Toronto",
            "remembered, as a message's is"
        );
        let too_often = agent
            .add_user_schedule("Every minute", "x", "* * * * *", None, None, false)
            .unwrap_err();
        assert!(too_often.contains("15 minutes"), "{too_often}");
        assert!(
            agent
                .add_user_schedule("Morning inbox", "y", "0 9 * * *", None, None, false)
                .unwrap_err()
                .contains("already")
        );
        assert!(
            agent
                .add_user_schedule("Bad", "y", "not a cron", None, None, false)
                .is_err()
        );
        assert!(
            agent
                .add_user_schedule("", "y", "0 9 * * *", None, None, false)
                .is_err()
        );
        assert_eq!(agent.store.schedules().unwrap().len(), 1);
        let created = agent.store.events_after(0, 100).unwrap();
        assert!(
            created
                .iter()
                .any(|e| e.kind == "schedule.created" && e.data["by"] == "me@acme.com")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn schedules_from_the_chat_run_without_asking_unless_outside_content_asked() {
        let (agent, dir) = test_agent();
        let asked = json!({"name": "Daily check", "prompt": "Check the build.", "cron": "0 9 * * *"});
        let made = agent.schedule_task("t1", &asked, false, None).unwrap();
        assert!(made.starts_with("scheduled #"), "{made}");
        let daily = agent.store.schedules().unwrap();
        assert!(daily[0].auto_approve, "asked for in a clean chat");

        // After a page or an email: a card first, and even once approved its
        // runs skip whatever would ask.
        let after = json!({"name": "Hourly sync", "prompt": "Sync it.", "cron": "0 * * * *"});
        let park = agent
            .schedule_task("t2", &after, true, None)
            .unwrap_err()
            .downcast::<Park>()
            .expect("asks first");
        let approved = Answer {
            approved: true,
            card: park.card,
            by: None,
            edited: None,
        };
        let made = agent.schedule_task("t2", &after, true, Some(approved)).unwrap();
        assert!(made.starts_with("scheduled #"), "{made}");
        let hourly = agent
            .store
            .schedules()
            .unwrap()
            .into_iter()
            .find(|s| s.name == "Hourly sync")
            .unwrap();
        assert!(!hourly.auto_approve);
        assert_eq!(hourly.created_from, "agent_tainted");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod background_tests {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use axum::Router;
    use axum::routing::post;

    use super::*;
    use crate::store::QueuedRun;

    /// What the stand-in model does with a scheduled run's request.
    #[derive(Clone, Copy, PartialEq)]
    enum Scheduled {
        /// Answers once released (`Notify`).
        Hold,
        /// Answers with a 400, which isn't retried.
        Fail,
    }

    /// A stand-in for the model on localhost: a scheduled run's request as
    /// `scheduled` says, anything else answered at once.
    async fn stand_in_model(scheduled: Scheduled) -> (String, Arc<Notify>) {
        let release = Arc::new(Notify::new());
        let held = release.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let held = held.clone();
                async move {
                    let is_scheduled = body["messages"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|m| m["content"].as_str().is_some_and(|c| c.contains("This is the scheduled task")));
                    let answer = |text: &str| {
                        axum::Json(json!({
                            "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 10, "completion_tokens": 2},
                        }))
                        .into_response()
                    };
                    match (is_scheduled, scheduled) {
                        (false, _) => answer("Hi there."),
                        (true, Scheduled::Hold) => {
                            held.notified().await;
                            answer("All quiet.")
                        }
                        (true, Scheduled::Fail) => {
                            (axum::http::StatusCode::BAD_REQUEST, "no").into_response()
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1/chat/completions", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, release)
    }

    use axum::response::IntoResponse;

    fn agent_on(model: &str) -> (Arc<Agent>, std::path::PathBuf) {
        let (agent, dir) = test_agent();
        let mut cfg = agent.cfg.clone();
        cfg.llm_url = model.to_string();
        drop(agent);
        (Arc::new(Agent::new(cfg).unwrap()), dir)
    }

    fn a_schedule(agent: &Agent) -> Schedule {
        let made = agent
            .add_user_schedule("Hourly check", "Check the build", "0 * * * *", None, None, true)
            .unwrap();
        agent
            .store
            .schedule(made["id"].as_i64().unwrap())
            .unwrap()
            .unwrap()
    }

    fn queue(agent: &Agent, schedule: &Schedule, task: &str) -> QueuedRun {
        let now = unix_now();
        assert!(
            agent
                .store
                .enqueue_run(
                    task,
                    schedule.id,
                    &schedule.prompt,
                    "zai-org/GLM-5.3",
                    "v",
                    now - 1,
                    0
                )
                .unwrap()
        );
        agent.store.claim_run(now).unwrap().unwrap()
    }

    fn events(agent: &Agent, kind: &str) -> Vec<Value> {
        agent
            .store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.data)
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

    #[tokio::test]
    async fn the_chat_stays_free_while_a_scheduled_run_works() {
        let (model, release) = stand_in_model(Scheduled::Hold).await;
        let (agent, dir) = agent_on(&model);
        let schedule = a_schedule(&agent);
        let queued = queue(&agent, &schedule, "sched1");
        let background = tokio::spawn(agent.clone().run_queued(queued, schedule));
        until("the run is in progress", || agent.background_run().is_some()).await;

        // The chat isn't busy, and a message starts a task at once. (It
        // fails without Chrome here; what matters is that it ran.)
        assert!(!agent.is_busy());
        let Ok(Posted::Started(chat)) = agent.post(MAIN, "Hi".into(), None) else {
            panic!("the chat is busy")
        };
        until("the chat's task ends", || !agent.is_busy()).await;
        let started: Vec<Value> = events(&agent, "task.started");
        assert_eq!(started.len(), 1);
        assert_eq!(started[0]["task"], chat);
        assert_eq!(events(&agent, "run.started")[0]["task"], "sched1");
        // Nothing of the run's in the chat yet.
        assert!(events(&agent, "message").iter().all(|m| m["task"] != "sched1"));

        release.notify_one();
        background.await.unwrap();
        // Its report, whole, after the chat: instructions, then the answer.
        let messages = events(&agent, "message");
        let n = messages.len();
        assert_eq!(messages[n - 2]["schedule"]["name"], "Hourly check");
        assert_eq!(messages[n - 2]["text"], "Check the build");
        assert_eq!(messages[n - 1]["text"], "All quiet.");
        assert_eq!(events(&agent, "run.finished")[0]["status"], "done");
        assert_eq!(events(&agent, "task.finished").len(), 1); // the chat's only
        let history = agent.store.history(10).unwrap();
        let roles: Vec<&str> = history.iter().map(|h| h.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        assert_eq!(history[0].text, "Hi");
        assert!(history[2].text.starts_with("[Scheduled task “Hourly check”]"));
        assert!(agent.background_run().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_report_that_ends_mid_question_waits_for_the_answer() {
        let (agent, dir) = test_agent();
        agent.open_thread(MAIN, "What's in my inbox?");
        agent.add_report_history(
            MAIN,
            "[Scheduled task “Hourly check”] Check the build".into(),
            "All quiet.",
        );
        assert_eq!(agent.store.history(10).unwrap().len(), 1);
        agent.close_thread(MAIN, Some("Two new emails."));
        let texts: Vec<String> = agent
            .store
            .history(10)
            .unwrap()
            .into_iter()
            .map(|h| h.text)
            .collect();
        assert_eq!(
            texts,
            [
                "What's in my inbox?",
                "Two new emails.",
                "[Scheduled task “Hourly check”] Check the build",
                "All quiet."
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_failed_scheduled_run_is_tried_once_more_quietly() {
        let (model, _release) = stand_in_model(Scheduled::Fail).await;
        let (agent, dir) = agent_on(&model);
        let schedule = a_schedule(&agent);
        let first = queue(&agent, &schedule, "try1");
        agent.clone().run_queued(first, schedule.clone()).await;
        // Nothing in the chat; a retry queued for a little later.
        assert!(events(&agent, "message").is_empty());
        assert_eq!(events(&agent, "run.finished")[0]["retry"], true);
        assert!(agent.store.claim_run(unix_now()).unwrap().is_none());
        let retry = agent
            .store
            .claim_run(unix_now() + RETRY_AFTER_SECS + 1)
            .unwrap()
            .unwrap();
        assert_eq!(retry.attempt, 1);
        // The retry fails too: that one is reported.
        agent.clone().run_queued(retry, schedule).await;
        let messages = events(&agent, "message");
        assert_eq!(messages.len(), 2);
        assert!(
            messages[1]["text"]
                .as_str()
                .unwrap()
                .starts_with("Something went wrong")
        );
        assert_eq!(agent.store.queued_count().unwrap(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn pausing_a_schedule_stops_its_run_and_drops_what_is_queued() {
        let (model, _release) = stand_in_model(Scheduled::Hold).await;
        let (agent, dir) = agent_on(&model);
        let schedule = a_schedule(&agent);
        let queued = queue(&agent, &schedule, "sched2");
        let background = tokio::spawn(agent.clone().run_queued(queued, schedule.clone()));
        until("the run is in progress", || agent.background_run().is_some()).await;
        assert!(agent.stop_schedule_runs(schedule.id));
        background.await.unwrap();
        let messages = events(&agent, "message");
        assert_eq!(messages.last().unwrap()["quiet"], true);
        assert_eq!(events(&agent, "run.finished")[0]["status"], "cancelled");
        // A run queued for it later is dropped too.
        let now = unix_now();
        assert!(
            agent
                .store
                .enqueue_run("sched3", schedule.id, "x", "m", "v", now, 0)
                .unwrap()
        );
        assert!(!agent.stop_schedule_runs(schedule.id));
        assert_eq!(agent.store.queued_count().unwrap(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn recovery_does_not_retry_a_run_with_a_recorded_action() {
        let (agent, dir) = test_agent();
        let schedule = a_schedule(&agent);
        let now = unix_now();
        agent
            .store
            .enqueue_run("acted", schedule.id, "x", "m", "v", now, 0)
            .unwrap();
        let run = agent.store.claim_run(now).unwrap().unwrap();
        assert!(agent.store.run_has_no_actions(&run.id).unwrap());
        agent.acted(&run.id).unwrap();

        let restarted = Agent::new(agent.cfg.clone()).unwrap();
        assert!(
            restarted
                .store
                .claim_run(now + RETRY_AFTER_SECS)
                .unwrap()
                .is_none()
        );
        assert!(events(&restarted, "message").iter().any(|message| {
            message["text"]
                .as_str()
                .unwrap_or_default()
                .contains("Check for changes before running it again")
        }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_run_cut_off_by_a_restart_is_queued_again_once() {
        let (agent, dir) = test_agent();
        let schedule = a_schedule(&agent);
        let now = unix_now();
        agent
            .store
            .enqueue_run("cut1", schedule.id, "x", "m", "v", now - 5, 0)
            .unwrap();
        agent.store.claim_run(now).unwrap().unwrap(); // running when agentd stopped
        let restarted = Agent::new(agent.cfg.clone()).unwrap();
        let retry = restarted
            .store
            .claim_run(unix_now())
            .unwrap()
            .expect("queued again");
        assert_eq!(retry.attempt, 1);
        assert_eq!(retry.schedule, schedule.id);
        // Cut off again: not a third time.
        let again = Agent::new(agent.cfg.clone()).unwrap();
        assert!(again.store.claim_run(now).unwrap().is_none());
        assert!(events(&again, "message").is_empty()); // quietly
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A stand-in model that answers each request with `reply(request)`, and
    /// keeps every request. A message with no content and no calls comes back
    /// cut off (finish "length"), as when the thinking used up the reply.
    async fn scripted_model(
        reply: impl Fn(&Value) -> Value + Send + Sync + 'static,
    ) -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let reply = Arc::new(reply);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let (reply, log) = (reply.clone(), log.clone());
                async move {
                    let message = reply(&body);
                    log.lock().unwrap().push(body);
                    let finish = if message["tool_calls"].is_array() {
                        "tool_calls"
                    } else if message["content"].is_null() {
                        "length"
                    } else {
                        "stop"
                    };
                    axum::Json(json!({
                        "choices": [{"message": message, "finish_reason": finish}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 2},
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1/chat/completions", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, seen)
    }

    fn is_review(request: &Value) -> bool {
        request["messages"][0]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("(agentd supervisor)"))
    }

    fn is_wrap_up(request: &Value) -> bool {
        request["tool_choice"] == "none"
    }

    /// The task's own turns (not the supervisor's, nor memory's tidying up,
    /// which may run after a task ends).
    fn is_turn(request: &Value) -> bool {
        request["messages"][0]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("You are an assistant"))
    }

    fn says(text: &str) -> Value {
        json!({"role": "assistant", "content": text})
    }

    fn cut_off() -> Value {
        json!({"role": "assistant", "content": null})
    }

    /// A call that works without a browser, and never fails.
    fn lists_schedules() -> Value {
        json!({"role": "assistant", "content": null, "tool_calls": [{
            "id": format!("c{}", short_id()), "type": "function",
            "function": {"name": "list_schedules", "arguments": "{}"},
        }]})
    }

    fn last_message(request: &Value) -> String {
        request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Runs the "Hourly check" schedule ("Check the build") once, to its end.
    async fn run_scheduled(model: &str) -> (Arc<Agent>, std::path::PathBuf) {
        let (agent, dir) = agent_on(model);
        let schedule = a_schedule(&agent);
        let queued = queue(&agent, &schedule, "sched1");
        agent.clone().run_queued(queued, schedule).await;
        (agent, dir)
    }

    #[tokio::test]
    async fn the_supervisor_stops_a_scheduled_run_that_is_stuck() {
        let (model, seen) = scripted_model(|request| {
            if is_review(request) {
                says(r#"{"verdict": "stuck", "reason": "It lists the schedules over and over.", "advice": ""}"#)
            } else if is_wrap_up(request) {
                says("I listed the schedules but never got to the build.")
            } else {
                lists_schedules()
            }
        })
        .await;
        let (agent, dir) = run_scheduled(&model).await;
        // A check every 5 steps here (test_agent): the first one finds it stuck.
        let finished = &events(&agent, "run.finished")[0];
        assert_eq!(
            (finished["status"].as_str(), finished["steps"].as_u64()),
            (Some("stuck"), Some(5))
        );
        let reviews = events(&agent, "review");
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0]["verdict"], "stuck");
        assert_eq!(reviews[0]["reason"], "It lists the schedules over and over.");
        let messages = events(&agent, "message");
        assert_eq!(
            messages.last().unwrap()["text"],
            "I listed the schedules but never got to the build."
        );
        // The supervisor read the request and every call since the start; the
        // wrap-up told the model why it had to stop.
        let seen = seen.lock().unwrap();
        let digest = seen.iter().find(|r| is_review(r)).unwrap()["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(digest.starts_with("The user's request:\nCheck the build"));
        assert_eq!(digest.matches("- list_schedules {}").count(), 5);
        let wrap_up = seen.iter().find(|r| is_turn(r) && is_wrap_up(r)).unwrap();
        assert!(last_message(wrap_up).starts_with("(agentd) A check on your progress found you stuck"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn long_runs_go_on_and_a_nudge_reaches_the_agent() {
        let reviews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = reviews.clone();
        let (model, seen) = scripted_model(move |request| {
            if is_review(request) {
                return match counted.fetch_add(1, Ordering::SeqCst) {
                    0 => says(r#"{"verdict": "progressing", "reason": "Making progress.", "advice": ""}"#),
                    _ => says(
                        r#"{"verdict": "nudge", "reason": "It keeps listing schedules.", "advice": "Check the build log instead."}"#,
                    ),
                };
            }
            if last_message(request).starts_with("(agentd) A check on your progress") {
                says("The build is green.")
            } else {
                lists_schedules()
            }
        })
        .await;
        let (agent, dir) = run_scheduled(&model).await;
        // Past where the old cap (5 steps here) would have ended it.
        let finished = &events(&agent, "run.finished")[0];
        assert_eq!(
            (finished["status"].as_str(), finished["steps"].as_u64()),
            (Some("done"), Some(11))
        );
        let verdicts: Vec<Value> = events(&agent, "review")
            .iter()
            .map(|r| r["verdict"].clone())
            .collect();
        assert_eq!(verdicts, [json!("progressing"), json!("nudge")]);
        assert_eq!(
            events(&agent, "message").last().unwrap()["text"],
            "The build is green."
        );
        let seen = seen.lock().unwrap();
        let nudged = seen.iter().rfind(|r| is_turn(r)).unwrap();
        assert_eq!(
            last_message(nudged),
            "(agentd) A check on your progress: It keeps listing schedules. Try this: Check the build log instead."
        );
        // The second check knew only the calls since the first.
        let second = seen.iter().filter(|r| is_review(r)).nth(1).unwrap()["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(second.matches("- list_schedules {}").count(), 5);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_check_that_keeps_failing_counts_as_stuck() {
        let (model, _) = scripted_model(|request| {
            if is_review(request) {
                says("I'd say it's fine.") // no verdict
            } else if is_wrap_up(request) {
                says("Stopped early.")
            } else {
                lists_schedules()
            }
        })
        .await;
        let (agent, dir) = run_scheduled(&model).await;
        let verdicts: Vec<Value> = events(&agent, "review")
            .iter()
            .map(|r| r["verdict"].clone())
            .collect();
        assert_eq!(verdicts, [json!("failed"), json!("failed"), json!("stuck")]);
        let finished = &events(&agent, "run.finished")[0];
        assert_eq!(
            (finished["status"].as_str(), finished["steps"].as_u64()),
            (Some("stuck"), Some(6))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_cut_off_reply_is_asked_again_with_more_room() {
        let turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = turns.clone();
        let (model, seen) = scripted_model(move |request| match counted.fetch_add(1, Ordering::SeqCst) {
            _ if !is_turn(request) => says("{}"),
            // A call, then twice cut off, then an answer.
            0 => lists_schedules(),
            1 | 2 => cut_off(),
            _ => says("All quiet."),
        })
        .await;
        let (agent, dir) = run_scheduled(&model).await;
        assert_eq!(events(&agent, "run.finished")[0]["status"], "done");
        assert_eq!(events(&agent, "message").last().unwrap()["text"], "All quiet.");
        let seen = seen.lock().unwrap();
        let turns: Vec<&Value> = seen.iter().filter(|r| is_turn(r)).collect();
        let limits: Vec<u64> = turns.iter().map(|r| r["max_tokens"].as_u64().unwrap()).collect();
        assert_eq!(limits, [8192, 8192, 16384, 32768]);
        assert!(last_message(turns[2]).starts_with("(agentd) Your last reply was cut off"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn replies_that_stay_cut_off_end_with_a_summary() {
        // Even the summary is cut off: the fallback says what happened.
        let (model, seen) = scripted_model(|_| cut_off()).await;
        let (agent, dir) = run_scheduled(&model).await;
        assert_eq!(events(&agent, "run.finished")[0]["status"], "stuck");
        let answer = events(&agent, "message").last().unwrap()["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            answer.starts_with("The model's replies kept getting cut off"),
            "{answer}"
        );
        let seen = seen.lock().unwrap();
        let turns: Vec<&Value> = seen.iter().filter(|r| is_turn(r)).collect();
        let limits: Vec<u64> = turns.iter().map(|r| r["max_tokens"].as_u64().unwrap()).collect();
        assert_eq!(limits, [8192, 16384, 32768, 8192]);
        assert!(is_wrap_up(turns[3]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_refused_larger_reply_falls_back_to_the_usual_limit() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|axum::Json(body): axum::Json<Value>| async move {
                match body["max_tokens"].as_u64() {
                    // As Together does when the account can't cover it.
                    Some(n) if n > 8192 => (axum::http::StatusCode::PAYMENT_REQUIRED, "credit").into_response(),
                    _ if body["messages"].as_array().unwrap().len() > 3 => axum::Json(json!({
                        "choices": [{"message": {"role": "assistant", "content": "All quiet."}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 2},
                    }))
                    .into_response(),
                    _ => axum::Json(json!({
                        "choices": [{"message": {"role": "assistant", "content": null}, "finish_reason": "length"}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 2},
                    }))
                    .into_response(),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model = format!("http://{}/v1/chat/completions", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (agent, dir) = run_scheduled(&model).await;
        assert_eq!(events(&agent, "run.finished")[0]["status"], "done");
        assert_eq!(events(&agent, "message").last().unwrap()["text"], "All quiet.");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn stop_on_a_stuck_card_ends_the_task_with_a_summary() {
        let (model, seen) = scripted_model(|request| {
            if is_wrap_up(request) {
                says("Done: cloned the repo. Left: the build, which kept failing.")
            } else {
                says("unexpected")
            }
        })
        .await;
        let (agent, dir) = agent_on(&model);
        agent
            .store
            .start_run(
                "t1",
                "Deploy the blog",
                None,
                "zai-org/GLM-5.3",
                &agent.policy_version(),
                None,
            )
            .unwrap();
        let state = json!({
            "task": "t1", "model": "zai-org/GLM-5.3", "allow_actions": true,
            "messages": [{"role": "system", "content": ""}, {"role": "user", "content": "Deploy the blog"}],
            "queue": [], "step": 70, "tainted": false, "reminded": false, "loops": guard::LoopDetector::default(),
            "usage": Usage::default(), "seconds": 2000.0, "request": "Deploy the blog",
            "review": {"at_step": 70, "at_secs": 2000.0, "at_message": 2, "failures": 0,
                       "previous": "stuck: The build fails the same way each time.",
                       "stuck": ["The build fails the same way each time.", "Read the error."]},
        });
        let card = json!({"kind": "stuck", "id": "s1", "task": "t1", "reason": "The build fails the same way each time.",
                          "minutes": 33, "steps": 70, "plan": null});
        agent
            .store
            .park("s1", "t1", &card, &state.to_string(), &agent.policy_version())
            .unwrap();
        assert!(
            agent
                .answer_approval("s1", false, None, Some("me".into()))
                .is_ok()
        );
        until("the task ends", || !events(&agent, "task.finished").is_empty()).await;
        assert_eq!(events(&agent, "task.finished")[0]["status"], "stuck");
        assert_eq!(
            events(&agent, "message").last().unwrap()["text"],
            "Done: cloned the repo. Left: the build, which kept failing."
        );
        let seen = seen.lock().unwrap();
        let turns: Vec<&Value> = seen.iter().filter(|r| is_turn(r)).collect();
        assert_eq!(turns.len(), 1);
        assert!(is_wrap_up(turns[0]));
        assert_eq!(
            last_message(turns[0]),
            tools::wrap_up(
                "The user stopped this task after a check found it stuck: The build fails the same way each time."
            )
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod access_tests {
    use super::*;

    /// An agent whose user-allowed sites go to a file in its state directory
    /// (no Squid to reload here: local development).
    fn agent_with_sites() -> (Arc<Agent>, std::path::PathBuf) {
        let (agent, dir) = test_agent();
        let mut cfg = agent.cfg.clone();
        cfg.egress_user_hosts = dir.join("allowed-hosts.txt").to_string_lossy().into_owned();
        drop(agent);
        (Arc::new(Agent::new(cfg).unwrap()), dir)
    }

    fn file(agent: &Agent) -> String {
        std::fs::read_to_string(&agent.cfg.egress_user_hosts).unwrap()
    }

    #[test]
    fn asking_for_a_site_shows_a_card_and_approving_opens_it() {
        let (agent, dir) = agent_with_sites();
        // Squid's file exists from the start, empty but for its note.
        assert!(file(&agent).starts_with('#'));
        let args = json!({"hosts": ["api.example.com", "https://api.github.com/repos"], "why": "to read the status API"});
        let park = agent
            .request_access("t1", &args, true, None, false)
            .unwrap_err()
            .downcast::<Park>()
            .expect("asks first");
        // .github.com is the deployment's already: only the new one is asked.
        assert_eq!(park.card["kind"], "access");
        assert_eq!(park.card["hosts"], json!(["api.example.com"]));
        assert_eq!(park.card["tainted"], true);
        let answer = Answer {
            approved: true,
            card: park.card,
            by: Some("me@acme.com".into()),
            edited: None,
        };
        let reply = agent
            .request_access("t1", &args, true, Some(answer), false)
            .unwrap();
        assert!(reply.starts_with("allowed:"), "{reply}");
        assert!(file(&agent).ends_with("api.example.com\n"));
        let saved = agent.store.egress_hosts().unwrap();
        assert_eq!(saved[0].by.as_deref(), Some("me@acme.com"));
        assert_eq!(saved[0].task.as_deref(), Some("t1"));
        assert_eq!(agent.access_list()["allowed"][0]["host"], "api.example.com");
        // Asked again: nothing to ask.
        let again = agent.request_access("t2", &args, false, None, false).unwrap();
        assert!(again.starts_with("already allowed"), "{again}");
        // Removed in Settings: gone from Squid's file too.
        assert!(agent.remove_host("api.example.com", Some("me@acme.com")).unwrap());
        assert!(!file(&agent).contains("api.example.com"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn auto_approve_cannot_change_the_proxy_allowlist() {
        let (agent, dir) = agent_with_sites();
        agent
            .set_auto_approve(true, Some(&agent.settings().auto_approve_version), None)
            .unwrap();
        let args = json!({"hosts": ["api.example.com"], "why": "Read its status"});
        let card = agent
            .request_access("t1", &args, true, None, false)
            .unwrap_err()
            .downcast::<Park>()
            .unwrap();
        assert_eq!(card.card["kind"], "access");
        assert!(agent.store.egress_hosts().unwrap().is_empty());
        assert!(
            agent
                .request_access("t2", &args, true, None, true)
                .unwrap()
                .contains("scheduled run can't ask")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn what_can_never_be_asked_for() {
        let (agent, dir) = agent_with_sites();
        let ask = |hosts: Value| {
            agent
                .request_access("t3", &json!({"hosts": hosts, "why": "x"}), false, None, false)
                .unwrap()
        };
        for hosts in [
            json!(["10.0.0.5"]),
            json!(["*.example.com"]),
            json!(["db.internal"]),
            json!([]),
            json!([
                "a.example.com",
                "b.example.com",
                "c.example.com",
                "d.example.com",
                "e.example.com",
                "f.example.com"
            ]),
        ] {
            assert!(ask(hosts.clone()).starts_with("not asked"), "{hosts}");
        }
        // A scheduled run can't ask, and a denied card opens nothing.
        let reply = agent
            .request_access("t4", &json!({"hosts": ["api.example.com"]}), false, None, true)
            .unwrap();
        assert!(
            reply.starts_with("not asked: a scheduled run can't ask"),
            "{reply}"
        );
        let card = json!({"kind": "access", "hosts": ["api.example.com"], "why": "", "tainted": false});
        let denied = Answer {
            approved: false,
            card,
            by: None,
            edited: None,
        };
        let reply = agent
            .request_access(
                "t5",
                &json!({"hosts": ["api.example.com"]}),
                false,
                Some(denied),
                false,
            )
            .unwrap();
        assert!(reply.starts_with("the user didn't allow"), "{reply}");
        assert!(agent.store.egress_hosts().unwrap().is_empty());
        assert!(events_of(&agent, "approval.requested").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn without_a_hosts_file_the_deployment_decides() {
        let (agent, dir) = test_agent();
        let reply = agent
            .request_access("t6", &json!({"hosts": ["api.example.com"]}), false, None, false)
            .unwrap();
        assert!(reply.contains("set by whoever deploys it"), "{reply}");
        assert_eq!(agent.access_list()["enabled"], false);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_refused_site_in_a_commands_output_says_how_to_ask() {
        let (agent, dir) = agent_with_sites();
        let (_stop, mut cancel) = watch::channel(false);
        let command = "echo 'curl: (56) CONNECT tunnel failed, response 403'";
        let chat = agent
            .run_shell(
                "t7",
                command,
                None,
                &[],
                10,
                false,
                false,
                false,
                false,
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(chat.contains("call request_access"), "{chat}");
        let scheduled = agent
            .run_shell(
                "t8",
                command,
                None,
                &[],
                10,
                false,
                false,
                true,
                true,
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(scheduled.contains("A scheduled run can't ask"), "{scheduled}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn events_of(agent: &Agent, kind: &str) -> Vec<Value> {
        agent
            .store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.data)
            .collect()
    }
}
