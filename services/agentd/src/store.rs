//! SQLite state: the event log the apps stream, each conversation's history
//! the model sees, runs (tasks) and what they wait on, queued messages, the
//! agent's memory, schedules, the saved keys' names and labels (their values
//! are files: secrets.rs), and settings.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

/// The first conversation, with everything from before conversations.
pub const MAIN: &str = "main";

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub data: Value,
    /// The conversation it's about (`data.session`); None for a global one.
    #[serde(skip)]
    pub session: Option<String>,
}

const EVENT_COLUMNS: &str = "seq, ts, kind, data, session";

fn event_row(row: &rusqlite::Row) -> rusqlite::Result<Event> {
    let data: String = row.get(3)?;
    let session: Option<String> = row.get(4)?;
    let mut data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
    // Events logged before conversations were tagged by the migration only:
    // the apps read it from the event itself.
    if let (Some(session), Some(fields)) = (&session, data.as_object_mut()) {
        fields
            .entry("session")
            .or_insert_with(|| Value::from(session.as_str()));
    }
    Ok(Event {
        seq: row.get(0)?,
        ts: row.get(1)?,
        kind: row.get(2)?,
        data,
        session,
    })
}

/// A conversation (`sessions` row).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    /// Unix seconds.
    pub created: i64,
    /// Its last message, queued message, title change or creation.
    pub updated: i64,
    pub archived: bool,
    /// When it was deleted; its row stays for its runs' titles.
    pub deleted: Option<i64>,
    /// Its browser window's tab (not `main`'s, which is the `tab_target` setting).
    pub tab_target: Option<String>,
}

const SESSION_COLUMNS: &str = "id, title, created, updated, archived, deleted, tab_target";

fn session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        title: row.get(1)?,
        created: row.get(2)?,
        updated: row.get(3)?,
        archived: row.get(4)?,
        deleted: row.get(5)?,
        tab_target: row.get(6)?,
    })
}

/// A message accepted but not started yet (`chat_queue`), with its raw text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QueuedMessage {
    pub id: String,
    #[serde(skip)]
    pub session: String,
    pub text: String,
    pub by: Option<String>,
    /// Unix seconds.
    pub created: i64,
}

fn queued_message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueuedMessage> {
    Ok(QueuedMessage {
        id: row.get(0)?,
        session: row.get(1)?,
        text: row.get(2)?,
        by: row.get(3)?,
        created: row.get(4)?,
    })
}

/// One run for the usage figures (usage.rs).
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRow {
    /// Unix seconds: a run counts on the day it started.
    pub started: i64,
    pub cost: f64,
    /// chat, scheduled or tidy_up.
    pub kind: String,
    pub schedule: Option<i64>,
    pub session: Option<String>,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub cached: i64,
    /// It ran (not a queued, skipped or never-started one).
    pub counted: bool,
}

/// A page of the event log, oldest first (`Store::events_page`).
#[derive(Debug)]
pub struct EventPage {
    pub events: Vec<Event>,
    /// Older events of those kinds remain.
    pub more: bool,
    /// The log's newest seq when the page was read.
    pub last_seq: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryEntry {
    pub ts: String,
    pub role: String,
    pub text: String,
}

pub struct Store {
    conn: Mutex<Connection>,
}

/// Something the agent remembers across conversations. Trusted when it came
/// from a task that hadn't read outside content (web pages, emails, command
/// output); unverified otherwise, until the user confirms it in the app.
#[derive(Clone, Debug, Serialize)]
pub struct Memory {
    pub id: i64,
    pub text: String,
    pub trusted: bool,
    pub created: String,
}

/// A task the agent starts on its own: repeating (`cron`, in `timezone`) or
/// once, at `next_run` (unix seconds).
#[derive(Clone, Debug, Serialize)]
pub struct Schedule {
    pub id: i64,
    pub name: String,
    pub prompt: String,
    pub cron: Option<String>,
    pub timezone: String,
    pub next_run: i64,
    pub last_run: Option<i64>,
    pub paused: bool,
    pub created: String,
    /// The rules it was set up under (policy.rs); None from before 0.15.
    pub policy_version: Option<String>,
    /// `user` (asked for in a clean task), `agent_clean`, or `agent_tainted`
    /// (approved through a card after outside content).
    pub created_from: String,
    /// "Run without asking": set by a person in the web app, never the agent.
    /// Its runs go ahead with what would otherwise ask, except root commands
    /// and browser changes (agent.rs).
    pub auto_approve: bool,
    /// The conversation it belongs to: its runs report there.
    pub session: String,
}

const SCHEDULE_COLUMNS: &str = "id, name, prompt, cron, timezone, next_run, last_run, paused, created, policy_version, created_from, auto_approve, session";

/// A site the user allowed the shell to reach (access.rs).
#[derive(Clone, Debug, Serialize)]
pub struct EgressHost {
    pub host: String,
    /// Unix seconds.
    pub added: i64,
    pub by: Option<String>,
    /// The task whose card asked; none when added in Settings.
    pub task: Option<String>,
    pub why: Option<String>,
}

/// A scheduled run in the queue (a `runs` row, `queued` until the background
/// worker takes it; schedule.rs).
#[derive(Clone, Debug, PartialEq)]
pub struct QueuedRun {
    pub id: String,
    pub schedule: i64,
    /// Unix seconds: not taken before then; how late it is counts from it.
    pub due: i64,
    /// 0 for the first try, 1 for the one retry (after an error or a restart).
    pub attempt: i64,
}

fn queued_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueuedRun> {
    Ok(QueuedRun {
        id: row.get(0)?,
        schedule: row.get(1)?,
        due: row.get(2)?,
        attempt: row.get(3)?,
    })
}

/// A browser where notifications are on (webpush.rs): its push endpoint and
/// keys. The web app lists them by label.
#[derive(Clone, Debug, Serialize)]
pub struct PushSubscription {
    /// The first 12 hex digits of the endpoint's SHA-256.
    pub id: String,
    pub endpoint: String,
    #[serde(skip)]
    pub p256dh: String,
    #[serde(skip)]
    pub auth: String,
    /// "Mac · Chrome": what the web app called it.
    pub label: String,
    /// Unix seconds.
    pub created: i64,
    pub last_sent: Option<i64>,
}

/// An iPhone or Mac app registered for APNs notifications (apns.rs). The token
/// stays on the desktop; the API lists devices by label only.
#[derive(Clone, Debug, Serialize)]
pub struct DeviceToken {
    /// The first 12 hex digits of the token's SHA-256.
    pub id: String,
    #[serde(skip)]
    pub token: String,
    /// The app's bundle id (the APNs topic).
    #[serde(skip)]
    pub topic: String,
    /// "sandbox" (Xcode builds) or "production" (TestFlight, notarized).
    pub environment: String,
    /// "iPhone 17", what the app called it.
    pub label: String,
    /// Unix seconds.
    pub created: i64,
    pub last_sent: Option<i64>,
}

/// A saved key, as the apps list it: never its value.
#[derive(Clone, Debug, Serialize)]
pub struct SecretInfo {
    /// The environment variable commands get it as, e.g. GITHUB_TOKEN.
    pub name: String,
    /// What it is, for people: "GitHub token".
    pub label: String,
    pub created: String,
    pub updated: String,
    pub last_used: Option<String>,
}

/// Something the user is asked: an approval a parked task waits on (with the
/// task's saved state), or an action-needed card.
#[derive(Clone, Debug)]
pub struct Pending {
    pub id: String,
    pub run: String,
    pub card: Value,
    pub checkpoint: Option<String>,
    /// Unix seconds.
    pub created: i64,
    /// The rules in force when it parked (policy.rs); None from before 0.15.
    pub policy_version: Option<String>,
}

/// A side effect waiting to be done (effects.rs).
#[derive(Clone, Debug, PartialEq)]
pub struct Effect {
    pub kind: String,
    pub key: String,
    pub payload: Value,
    /// The payload as stored: a finished run removes the row only if it's
    /// still this one (not enqueued again meanwhile).
    raw: String,
    /// Unix seconds.
    pub due: i64,
    /// Failures so far.
    pub attempts: i64,
    pub last_error: Option<String>,
}

/// After this many failures an effect gives up: it stays, and `/status`
/// reports it.
pub const EFFECT_MAX_ATTEMPTS: i64 = 10;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `Store::enqueue_effect`, for use inside a caller's transaction.
pub fn enqueue_effect(conn: &Connection, kind: &str, key: &str, payload: &Value, due: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO effects (kind, key, payload, due, created) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(kind, key) DO UPDATE SET payload = excluded.payload, due = excluded.due,
           attempts = CASE WHEN attempts >= ?6 THEN 0 ELSE attempts END,
           last_error = CASE WHEN attempts >= ?6 THEN NULL ELSE last_error END",
        params![
            kind,
            key,
            payload.to_string(),
            due,
            unix_now(),
            EFFECT_MAX_ATTEMPTS
        ],
    )?;
    Ok(())
}

fn effect_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Effect> {
    let raw: String = row.get(2)?;
    Ok(Effect {
        kind: row.get(0)?,
        key: row.get(1)?,
        payload: serde_json::from_str(&raw).unwrap_or(Value::Null),
        raw,
        due: row.get(3)?,
        attempts: row.get(4)?,
        last_error: row.get(5)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS events (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts TEXT NOT NULL DEFAULT ({NOW}),
                 kind TEXT NOT NULL,
                 data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS history (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts TEXT NOT NULL DEFAULT ({NOW}),
                 role TEXT NOT NULL,
                 content TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS runs (
                 id TEXT PRIMARY KEY,
                 agent TEXT NOT NULL DEFAULT 'main',
                 status TEXT NOT NULL,
                 request TEXT NOT NULL,
                 by_user TEXT,
                 model TEXT NOT NULL,
                 started TEXT NOT NULL DEFAULT ({NOW}),
                 updated TEXT NOT NULL DEFAULT ({NOW}),
                 steps INTEGER NOT NULL DEFAULT 0,
                 cost REAL,
                 answer TEXT);
             CREATE TABLE IF NOT EXISTS pending (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 run TEXT NOT NULL,
                 card TEXT NOT NULL DEFAULT '{{}}',
                 checkpoint TEXT,
                 created INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)));
             CREATE TABLE IF NOT EXISTS memory (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 agent TEXT NOT NULL DEFAULT 'main',
                 text TEXT NOT NULL,
                 trusted INTEGER NOT NULL,
                 created TEXT NOT NULL DEFAULT ({NOW}));
             CREATE TABLE IF NOT EXISTS schedules (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 agent TEXT NOT NULL DEFAULT 'main',
                 name TEXT NOT NULL,
                 prompt TEXT NOT NULL,
                 cron TEXT,
                 timezone TEXT NOT NULL,
                 next_run INTEGER NOT NULL,
                 last_run INTEGER,
                 paused INTEGER NOT NULL DEFAULT 0,
                 created TEXT NOT NULL DEFAULT ({NOW}));
             CREATE TABLE IF NOT EXISTS secrets (
                 name TEXT PRIMARY KEY,
                 agent TEXT NOT NULL DEFAULT 'main',
                 label TEXT NOT NULL,
                 created TEXT NOT NULL DEFAULT ({NOW}),
                 updated TEXT NOT NULL DEFAULT ({NOW}),
                 last_used TEXT);
             -- Slack and Teams updates (destinations, posts): unused here,
             -- kept so a database moves between versions with and without them.
             CREATE TABLE IF NOT EXISTS destinations (
                 name TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 channel TEXT NOT NULL DEFAULT '',
                 link TEXT NOT NULL DEFAULT '',
                 created INTEGER NOT NULL,
                 created_by TEXT,
                 last_post INTEGER,
                 last_error TEXT);
             CREATE TABLE IF NOT EXISTS posts (
                 id TEXT PRIMARY KEY,
                 destination TEXT NOT NULL,
                 title TEXT NOT NULL,
                 hash TEXT NOT NULL,
                 ts TEXT,
                 task TEXT,
                 created INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS posts_by_destination ON posts (destination, created);
             -- Browsers to send notifications to (webpush.rs).
             CREATE TABLE IF NOT EXISTS push_subscriptions (
                 id TEXT PRIMARY KEY,
                 endpoint TEXT NOT NULL UNIQUE,
                 p256dh TEXT NOT NULL,
                 auth TEXT NOT NULL,
                 label TEXT NOT NULL DEFAULT '',
                 created INTEGER NOT NULL,
                 last_sent INTEGER);
             -- iPhone and Mac apps to send notifications to (apns.rs).
             CREATE TABLE IF NOT EXISTS device_tokens (
                 id TEXT PRIMARY KEY,
                 token TEXT NOT NULL UNIQUE,
                 topic TEXT NOT NULL,
                 environment TEXT NOT NULL DEFAULT 'production',
                 label TEXT NOT NULL DEFAULT '',
                 created INTEGER NOT NULL,
                 last_sent INTEGER);"
        ))?;
        migrate(&conn)?;
        reconcile_legacy_memory_provenance(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Runs `f` in one transaction: committed if it returns Ok, rolled back
    /// otherwise.
    pub fn transact<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Runs `f` on the connection (reads).
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.conn.lock().unwrap())
    }

    // --- Runs ---------------------------------------------------------------------

    /// A task starts in `main`, under the rules in `policy_version`
    /// (policy.rs); `schedule` is set for a scheduled run.
    #[cfg(test)]
    pub fn start_run(
        &self,
        id: &str,
        request: &str,
        by: Option<&str>,
        model: &str,
        policy_version: &str,
        schedule: Option<i64>,
    ) -> Result<()> {
        self.start_run_in(MAIN, id, request, by, model, policy_version, schedule)
    }

    /// `start_run` in a conversation.
    #[allow(clippy::too_many_arguments)]
    pub fn start_run_in(
        &self,
        session: &str,
        id: &str,
        request: &str,
        by: Option<&str>,
        model: &str,
        policy_version: &str,
        schedule: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO runs (id, status, request, by_user, model, policy_version, schedule, acted, session)
             VALUES (?1, 'running', ?2, ?3, ?4, ?5, ?6, 0, ?7)",
            params![id, request, by, model, policy_version, schedule, session],
        )?;
        Ok(())
    }

    /// The conversation a run belongs to (none for a tidy-up).
    pub fn run_session(&self, id: &str) -> Result<Option<String>> {
        self.read(|conn| {
            Ok(conn
                .query_row("SELECT session FROM runs WHERE id = ?1", params![id], |row| {
                    row.get(0)
                })
                .optional()?
                .flatten())
        })
    }

    /// What a run has cost so far, written after each model call so a
    /// running task counts in the usage figures and the budget.
    pub fn record_usage(
        &self,
        id: &str,
        cost: Option<f64>,
        usage: &crate::llm::Usage,
        steps: u32,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE runs SET cost = ?2, input_tokens = ?3, output_tokens = ?4, cached_tokens = ?5,
               steps = MAX(steps, ?6) WHERE id = ?1",
            params![
                id,
                cost,
                usage.input as i64,
                usage.output as i64,
                usage.cached as i64,
                steps
            ],
        )?;
        Ok(())
    }

    /// What every run started at or after `from` (`iso`) cost.
    pub fn cost_since(&self, from: &str) -> Result<f64> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(SUM(cost), 0) FROM runs WHERE started >= ?1",
                params![from],
                |row| row.get(0),
            )?)
        })
    }

    /// What every run ever cost.
    pub fn cost_all_time(&self) -> Result<f64> {
        self.read(|conn| Ok(conn.query_row("SELECT COALESCE(SUM(cost), 0) FROM runs", [], |row| row.get(0))?))
    }

    /// The runs started at or after `from` (`iso`), for the usage figures.
    pub fn usage_since(&self, from: &str) -> Result<Vec<UsageRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT CAST(strftime('%s', started) AS INTEGER), COALESCE(cost, 0),
               CASE WHEN kind = 'consolidation' THEN 'tidy_up' WHEN schedule IS NULL THEN 'chat' ELSE 'scheduled' END,
               schedule, CASE WHEN kind = 'task' THEN COALESCE(session, 'main') END, model,
               COALESCE(input_tokens, 0), COALESCE(output_tokens, 0), COALESCE(cached_tokens, 0),
               status NOT IN ('queued', 'skipped') AND (steps > 0 OR COALESCE(cost, 0) > 0 OR kind = 'consolidation')
             FROM runs WHERE started >= ?1 ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![from], |row| {
            Ok(UsageRow {
                started: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                cost: row.get(1)?,
                kind: row.get(2)?,
                schedule: row.get(3)?,
                session: row.get(4)?,
                model: row.get(5)?,
                input: row.get(6)?,
                output: row.get(7)?,
                cached: row.get(8)?,
                counted: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The task changed something: ran a command, wrote in the browser, or
    /// set up or cancelled a schedule.
    pub fn mark_run_acted(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE runs SET acted = 1 WHERE id = ?1 AND acted = 0",
            params![id],
        )?;
        Ok(())
    }

    /// Only a run with a durable record of zero dispatched actions may be retried.
    /// Older records with unknown action status are treated conservatively.
    pub fn run_has_no_actions(&self, id: &str) -> Result<bool> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE id = ?1 AND acted = 0)",
                params![id],
                |row| row.get(0),
            )?)
        })
    }

    pub fn set_run_status(&self, id: &str, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!("UPDATE runs SET status = ?2, updated = {NOW} WHERE id = ?1"),
            params![id, status],
        )?;
        Ok(())
    }

    pub fn finish_run(
        &self,
        id: &str,
        status: &str,
        steps: u32,
        cost: Option<f64>,
        answer: &str,
    ) -> Result<()> {
        self.finish_run_and_enqueue(id, status, steps, cost, answer, false)
    }

    pub fn finish_run_and_enqueue(
        &self,
        id: &str,
        status: &str,
        steps: u32,
        cost: Option<f64>,
        answer: &str,
        consolidate: bool,
    ) -> Result<()> {
        self.transact(|tx| {
            let changed = tx.execute(
                &format!(
                    "UPDATE runs SET status = ?2, steps = ?3, cost = ?4, answer = ?5, updated = {NOW} WHERE id = ?1"
                ),
                params![id, status, steps, cost, answer],
            )?;
            if changed > 0 && consolidate && status != "cancelled" {
                crate::consolidate::enqueue_after_run(tx, id)?;
            }
            Ok(())
        })
    }

    /// A schedule's run joins the queue, due at `due`, unless the schedule
    /// already has one queued or running (then nothing is added: false).
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_run(
        &self,
        id: &str,
        schedule: i64,
        request: &str,
        model: &str,
        policy_version: &str,
        due: i64,
        attempt: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        // It belongs to its schedule's conversation, where its report goes.
        Ok(conn.execute(
            "INSERT INTO runs (id, status, request, by_user, model, policy_version, schedule, acted, due, attempt, session)
             SELECT ?1, 'queued', ?2, 'schedule', ?3, ?4, ?5, 0, ?6, ?7,
               COALESCE((SELECT session FROM schedules WHERE id = ?5), 'main')
             WHERE NOT EXISTS (SELECT 1 FROM runs WHERE schedule = ?5 AND status IN ('queued', 'running'))",
            params![id, request, model, policy_version, schedule, due, attempt],
        )? > 0)
    }

    /// Takes the queued run due first (by `now`): it's `running` from here.
    pub fn claim_run(&self, now: i64) -> Result<Option<QueuedRun>> {
        let conn = self.conn.lock().unwrap();
        let next = conn
            .query_row(
                "SELECT id, schedule, due, attempt FROM runs
                 WHERE status = 'queued' AND due <= ?1 ORDER BY due, rowid LIMIT 1",
                params![now],
                queued_row,
            )
            .optional()?;
        if let Some(run) = &next {
            conn.execute(
                &format!(
                    "UPDATE runs SET status = 'running', started = {NOW}, updated = {NOW} WHERE id = ?1"
                ),
                params![run.id],
            )?;
        }
        Ok(next)
    }

    /// When the next queued run is due.
    pub fn next_queued(&self) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        Ok(
            conn.query_row("SELECT MIN(due) FROM runs WHERE status = 'queued'", [], |row| {
                row.get(0)
            })?,
        )
    }

    /// How many runs wait in the queue.
    pub fn queued_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(
            conn.query_row("SELECT COUNT(*) FROM runs WHERE status = 'queued'", [], |row| {
                row.get(0)
            })?,
        )
    }

    /// A schedule's queued runs are cancelled (it was paused or deleted).
    pub fn cancel_queued_runs(&self, schedule: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            &format!("UPDATE runs SET status = 'cancelled', updated = {NOW} WHERE status = 'queued' AND schedule = ?1"),
            params![schedule],
        )?)
    }

    /// Scheduled runs left `running` (agentd stopped during them).
    pub fn running_scheduled_runs(&self) -> Result<Vec<QueuedRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, schedule, COALESCE(due, 0), attempt FROM runs
             WHERE status = 'running' AND schedule IS NOT NULL ORDER BY rowid",
        )?;
        let rows = stmt.query_map([], queued_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A scheduled run that was queued: it starts now, on this model and
    /// these rules. False if there's no such row (it wasn't queued).
    pub fn begin_queued_run(&self, id: &str, model: &str, policy_version: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            &format!("UPDATE runs SET model = ?2, policy_version = ?3, updated = {NOW} WHERE id = ?1"),
            params![id, model, policy_version],
        )? > 0)
    }

    // --- Sites the user allowed (access.rs) -----------------------------------------

    /// The hosts, alphabetically.
    pub fn egress_hosts(&self) -> Result<Vec<EgressHost>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT host, added, by_user, task, why FROM egress_hosts ORDER BY host")?;
        let rows = stmt.query_map([], |row| {
            Ok(EgressHost {
                host: row.get(0)?,
                added: row.get(1)?,
                by: row.get(2)?,
                task: row.get(3)?,
                why: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds hosts (ones already there keep their record). How many were new.
    pub fn allow_hosts(
        &self,
        hosts: &[String],
        by: Option<&str>,
        task: Option<&str>,
        why: Option<&str>,
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut added = 0;
        for host in hosts {
            added += conn.execute(
                "INSERT OR IGNORE INTO egress_hosts (host, added, by_user, task, why) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![host, unix_now(), by, task, why],
            )?;
        }
        Ok(added)
    }

    pub fn remove_host(&self, host: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM egress_hosts WHERE host = ?1", params![host])? > 0)
    }

    /// IDs of the runs with this status, oldest first.
    pub fn runs_with_status(&self, status: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM runs WHERE status = ?1 ORDER BY started")?;
        let rows = stmt.query_map(params![status], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- Memory -------------------------------------------------------------------

    #[cfg(test)]
    pub fn remember(&self, text: &str, trusted: bool) -> Result<Memory> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "INSERT INTO memory (text, trusted) VALUES (?1, ?2) RETURNING id, text, trusted, created",
            params![text, trusted],
            memory_row,
        )?)
    }

    /// The latest `limit` memories, oldest first.
    pub fn memories(&self, limit: i64) -> Result<Vec<Memory>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, text, trusted, created FROM
               (SELECT id, text, trusted, created FROM memory WHERE agent = 'main' AND status = 'active'
                  AND (valid_until IS NULL OR valid_until > ?2) ORDER BY id DESC LIMIT ?1)
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![limit, unix_now()], memory_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    #[cfg(test)]
    pub fn forget(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM memory WHERE id = ?1", params![id])? > 0)
    }

    /// The user vouches for an unverified memory.
    #[cfg(test)]
    pub fn confirm_memory(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("UPDATE memory SET trusted = 1 WHERE id = ?1", params![id])? > 0)
    }

    // --- Schedules ----------------------------------------------------------------

    pub fn add_schedule(
        &self,
        name: &str,
        prompt: &str,
        cron: Option<&str>,
        timezone: &str,
        next_run: i64,
    ) -> Result<Schedule> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            &format!(
                "INSERT INTO schedules (name, prompt, cron, timezone, next_run) VALUES (?1, ?2, ?3, ?4, ?5)
                 RETURNING {SCHEDULE_COLUMNS}"
            ),
            params![name, prompt, cron, timezone, next_run],
            schedule_row,
        )?)
    }

    /// Where a new schedule came from, and the rules it was set up under.
    pub fn set_schedule_origin(&self, id: i64, policy_version: &str, created_from: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE schedules SET policy_version = ?2, created_from = ?3 WHERE id = ?1",
            params![id, policy_version, created_from],
        )?;
        Ok(())
    }

    /// A schedule runs under today's rules from now on.
    pub fn set_schedule_policy(&self, id: i64, policy_version: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE schedules SET policy_version = ?2 WHERE id = ?1",
            params![id, policy_version],
        )?;
        Ok(())
    }

    pub fn schedules(&self) -> Result<Vec<Schedule>> {
        self.schedules_where("1 = 1 ORDER BY id", params![])
    }

    pub fn schedule(&self, id: i64) -> Result<Option<Schedule>> {
        Ok(self.schedules_where("id = ?1", params![id])?.pop())
    }

    /// Schedules whose run is due, oldest first (paused ones aren't).
    pub fn due_schedules(&self, now: i64) -> Result<Vec<Schedule>> {
        self.schedules_where("paused = 0 AND next_run <= ?1 ORDER BY next_run", params![now])
    }

    /// When the next run is due.
    pub fn next_due(&self) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT MIN(next_run) FROM schedules WHERE paused = 0",
            [],
            |row| row.get(0),
        )?)
    }

    /// A run started (`ran`) or was skipped; `next_run` is the one after.
    pub fn schedule_ran(&self, id: i64, ran: Option<i64>, next_run: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE schedules SET next_run = ?2, last_run = COALESCE(?3, last_run) WHERE id = ?1",
            params![id, next_run, ran],
        )?;
        Ok(())
    }

    /// Pauses or resumes; a resumed schedule runs next at `next_run`.
    pub fn set_schedule_paused(&self, id: i64, paused: bool, next_run: Option<i64>) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE schedules SET paused = ?2, next_run = COALESCE(?3, next_run) WHERE id = ?1",
            params![id, paused, next_run],
        )? > 0)
    }

    /// "Run without asking" on or off; false if there's no such schedule.
    pub fn set_schedule_auto_approve(&self, id: i64, on: bool) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE schedules SET auto_approve = ?2 WHERE id = ?1",
            params![id, on],
        )? > 0)
    }

    pub fn delete_schedule(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM schedules WHERE id = ?1", params![id])? > 0)
    }

    /// The conversation a schedule reports to (the one that set it up).
    pub fn set_schedule_session(&self, id: i64, session: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE schedules SET session = ?2 WHERE id = ?1",
            params![id, session],
        )?;
        Ok(())
    }

    // --- Conversations (sessions) ---------------------------------------------------

    pub fn create_session(&self, id: &str, title: &str, now: i64) -> Result<SessionRow> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            &format!(
                "INSERT INTO sessions (id, title, created, updated) VALUES (?1, ?2, ?3, ?3)
                 RETURNING {SESSION_COLUMNS}"
            ),
            params![id, title, now],
            session_row,
        )?)
    }

    /// A conversation, deleted ones included.
    pub fn session(&self, id: &str) -> Result<Option<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                params![id],
                session_row,
            )
            .optional()?)
    }

    /// The archived conversations or the others (never deleted ones), most
    /// recently updated first.
    pub fn sessions(&self, archived: bool) -> Result<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE deleted IS NULL AND archived = ?1
             ORDER BY updated DESC, created DESC, rowid DESC"
        ))?;
        let rows = stmt.query_map(params![archived], session_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every conversation's title, deleted ones included (the usage figures).
    pub fn session_titles(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, title FROM sessions")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_session_title(&self, id: &str, title: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET title = ?2, updated = MAX(updated, ?3) WHERE id = ?1",
            params![id, title, now],
        )?;
        Ok(())
    }

    pub fn set_session_archived(&self, id: &str, archived: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET archived = ?2 WHERE id = ?1",
            params![id, archived],
        )?;
        Ok(())
    }

    /// The tab of a conversation's browser window (None: it has none).
    pub fn set_session_tab(&self, id: &str, target: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET tab_target = ?2 WHERE id = ?1",
            params![id, target],
        )?;
        Ok(())
    }

    /// The tabs of every conversation's own window (not `main`'s).
    pub fn session_tabs(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, tab_target FROM sessions WHERE tab_target IS NOT NULL AND deleted IS NULL AND id <> 'main'",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes a conversation: its history, queued messages and open Action
    /// needed cards go, its row stays (marked deleted) so its runs keep their
    /// title, and its schedules report to `main` from now on.
    pub fn delete_session(&self, id: &str, now: i64) -> Result<()> {
        self.transact(|tx| {
            tx.execute(
                "UPDATE sessions SET deleted = ?2, tab_target = NULL WHERE id = ?1",
                params![id, now],
            )?;
            tx.execute(
                "DELETE FROM pending WHERE kind = 'action' AND run IN (SELECT id FROM runs WHERE session = ?1)",
                params![id],
            )?;
            tx.execute("DELETE FROM history WHERE session = ?1", params![id])?;
            tx.execute("DELETE FROM chat_queue WHERE session = ?1", params![id])?;
            tx.execute(
                "UPDATE schedules SET session = 'main' WHERE session = ?1",
                params![id],
            )?;
            Ok(())
        })
    }

    /// A conversation's newest history entry: (role, text, unix seconds).
    pub fn last_message(&self, session: &str) -> Result<Option<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT role, content, COALESCE(CAST(strftime('%s', ts) AS INTEGER), 0) FROM history
                 WHERE session = ?1 ORDER BY id DESC LIMIT 1",
                params![session],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?)
    }

    // --- Queued messages ------------------------------------------------------------

    /// Queues a message; its position in its conversation's queue (1 = next).
    pub fn queue_message(
        &self,
        id: &str,
        session: &str,
        text: &str,
        by: Option<&str>,
        now: i64,
    ) -> Result<i64> {
        self.transact(|tx| {
            tx.execute(
                "INSERT INTO chat_queue (id, session, text, by_user, created) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, session, text, by, now],
            )?;
            tx.execute(
                "UPDATE sessions SET updated = MAX(updated, ?2) WHERE id = ?1",
                params![session, now],
            )?;
            Ok(tx.query_row(
                "SELECT COUNT(*) FROM chat_queue WHERE session = ?1",
                params![session],
                |row| row.get(0),
            )?)
        })
    }

    /// A conversation's queued messages, oldest first.
    pub fn queued_in(&self, session: &str) -> Result<Vec<QueuedMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session, text, by_user, created FROM chat_queue WHERE session = ?1 ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![session], queued_message_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Each conversation's oldest queued message, oldest first: what starts
    /// next (archived and deleted conversations wait).
    pub fn queue_heads(&self) -> Result<Vec<QueuedMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT q.id, q.session, q.text, q.by_user, q.created FROM chat_queue q
             JOIN sessions s ON s.id = q.session
             WHERE s.archived = 0 AND s.deleted IS NULL
               AND q.rowid = (SELECT MIN(rowid) FROM chat_queue WHERE session = q.session)
             ORDER BY q.rowid",
        )?;
        let rows = stmt.query_map([], queued_message_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// How many messages are queued, in all conversations.
    pub fn queued_messages(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM chat_queue q JOIN sessions s ON s.id = q.session WHERE s.deleted IS NULL",
            [],
            |row| row.get(0),
        )?)
    }

    /// Takes a queued message to start it; false if it's gone already.
    pub fn take_queued(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM chat_queue WHERE id = ?1", params![id])? > 0)
    }

    /// Removes one queued message; false if it's gone (started or removed).
    pub fn remove_queued(&self, session: &str, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM chat_queue WHERE id = ?1 AND session = ?2",
            params![id, session],
        )? > 0)
    }

    /// Drops a conversation's queue; the ids dropped, oldest first.
    pub fn drop_queue(&self, session: &str) -> Result<Vec<String>> {
        self.transact(|tx| {
            let ids = {
                let mut stmt = tx.prepare("SELECT id FROM chat_queue WHERE session = ?1 ORDER BY rowid")?;
                stmt.query_map(params![session], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?
            };
            tx.execute("DELETE FROM chat_queue WHERE session = ?1", params![session])?;
            Ok(ids)
        })
    }

    fn schedules_where(&self, condition: &str, args: impl rusqlite::Params) -> Result<Vec<Schedule>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE {condition}"
        ))?;
        let rows = stmt.query_map(args, schedule_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- Saved keys (names and labels; the values are files) -----------------------

    /// Records a key saved or replaced; a replacement keeps its first date.
    pub fn save_secret(&self, name: &str, label: &str) -> Result<SecretInfo> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!(
                "INSERT INTO secrets (name, label) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET label = excluded.label, updated = {NOW}"
            ),
            params![name, label],
        )?;
        Ok(conn.query_row(
            "SELECT name, label, created, updated, last_used FROM secrets WHERE name = ?1",
            params![name],
            secret_row,
        )?)
    }

    pub fn secrets(&self) -> Result<Vec<SecretInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT name, label, created, updated, last_used FROM secrets ORDER BY name")?;
        let rows = stmt.query_map([], secret_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_secret(&self, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM secrets WHERE name = ?1", params![name])? > 0)
    }

    pub fn secrets_used(&self, names: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for name in names {
            conn.execute(
                &format!("UPDATE secrets SET last_used = {NOW} WHERE name = ?1"),
                params![name],
            )?;
        }
        Ok(())
    }

    // --- Effects: side effects stored first, done afterwards (effects.rs) ----------

    /// Adds an effect, or replaces the payload and due time of the waiting one
    /// with that kind and key (its attempts carry over, unless it had given up:
    /// then it starts over).
    pub fn enqueue_effect(&self, kind: &str, key: &str, payload: &Value, due: i64) -> Result<()> {
        enqueue_effect(&self.conn.lock().unwrap(), kind, key, payload, due)
    }

    /// Adds an effect unless one with that kind and key is already there.
    pub fn ensure_effect(&self, kind: &str, key: &str, payload: &Value, due: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO effects (kind, key, payload, due, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, key, payload.to_string(), due, unix_now()],
        )?;
        Ok(())
    }

    /// Effects due by `now` that haven't given up, oldest first.
    pub fn due_effects(&self, now: i64, limit: i64) -> Result<Vec<Effect>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT kind, key, payload, due, attempts, last_error FROM effects
             WHERE due <= ?1 AND attempts < ?2 ORDER BY due, created LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![now, EFFECT_MAX_ATTEMPTS, limit], effect_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// When the next effect that hasn't given up is due.
    pub fn next_effect_due(&self) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT MIN(due) FROM effects WHERE attempts < ?1",
            params![EFFECT_MAX_ATTEMPTS],
            |row| row.get(0),
        )?)
    }

    /// Done: removes the effect, unless it was enqueued again while it ran (a
    /// new payload or due time), which then runs too.
    pub fn effect_done(&self, effect: &Effect) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM effects WHERE kind = ?1 AND key = ?2 AND payload = ?3 AND due = ?4",
            params![effect.kind, effect.key, effect.raw, effect.due],
        )?;
        Ok(())
    }

    /// Runs the effect again at `due`; a failure counts toward giving up.
    pub fn effect_again(&self, effect: &Effect, due: i64, error: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE effects SET due = ?5, attempts = attempts + ?6, last_error = COALESCE(?7, last_error)
             WHERE kind = ?1 AND key = ?2 AND payload = ?3 AND due = ?4",
            params![
                effect.kind,
                effect.key,
                effect.raw,
                effect.due,
                due,
                i64::from(error.is_some()),
                error
            ],
        )?;
        Ok(())
    }

    /// How many effects are waiting, and the ones that gave up.
    pub fn effects_health(&self) -> Result<(i64, Vec<Effect>)> {
        let conn = self.conn.lock().unwrap();
        let waiting: i64 = conn.query_row(
            "SELECT COUNT(*) FROM effects WHERE attempts < ?1",
            params![EFFECT_MAX_ATTEMPTS],
            |row| row.get(0),
        )?;
        let mut stmt = conn.prepare(
            "SELECT kind, key, payload, due, attempts, last_error FROM effects
             WHERE attempts >= ?1 ORDER BY kind, key",
        )?;
        let failing = stmt
            .query_map(params![EFFECT_MAX_ATTEMPTS], effect_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok((waiting, failing))
    }

    /// Hard-deletes memories deleted before `cutoff` (unix seconds): their
    /// rows, their full-text entries, and the text in their history.
    pub fn purge_deleted_memories(&self, cutoff: i64) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let gone = "SELECT id FROM memory WHERE status = 'deleted' AND COALESCE(updated, 0) < ?1";
        tx.execute(
            &format!("UPDATE memory_events SET before = NULL, after = NULL WHERE memory_id IN ({gone})"),
            params![cutoff],
        )?;
        tx.execute(
            &format!(
                "INSERT INTO memory_events (memory_id, actor, action, created)
                 SELECT id, 'system', 'purge', ?2 FROM ({gone})"
            ),
            params![cutoff, unix_now()],
        )?;
        let purged = tx.execute(
            "DELETE FROM memory WHERE status = 'deleted' AND COALESCE(updated, 0) < ?1",
            params![cutoff],
        )?;
        tx.execute(
            "DELETE FROM memory_fts WHERE id NOT IN (SELECT id FROM memory)",
            [],
        )?;
        tx.commit()?;
        Ok(purged)
    }

    // --- What the user is asked ---------------------------------------------------

    /// Parks a run on an approval: the card the user sees and the run's state.
    /// Parks a run on an approval, under the rules in `policy_version`, which
    /// are checked again before it resumes.
    pub fn park(
        &self,
        id: &str,
        run: &str,
        card: &Value,
        checkpoint: &str,
        policy_version: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pending (id, kind, run, card, checkpoint, policy_version)
             VALUES (?1, 'approval', ?2, ?3, ?4, ?5)",
            params![id, run, card.to_string(), checkpoint, policy_version],
        )?;
        Ok(())
    }

    /// Every parked approval: (id, card kind, conversation), newest first.
    pub fn parked_cards(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.id, COALESCE(json_extract(p.card, '$.kind'), 'shell'), COALESCE(r.session, 'main')
             FROM pending p LEFT JOIN runs r ON r.id = p.run
             WHERE p.kind = 'approval' ORDER BY p.created DESC, p.rowid DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The conversations with a task parked on a card: waiting.
    pub fn waiting_sessions(&self) -> Result<HashSet<String>> {
        Ok(self
            .parked_cards()?
            .into_iter()
            .map(|(_, _, session)| session)
            .collect())
    }

    /// Whether a conversation waits on a card.
    pub fn waiting_in(&self, session: &str) -> Result<bool> {
        Ok(self.parked_in(session)?.is_some())
    }

    /// The latest card a conversation's parked task waits on.
    pub fn parked_in(&self, session: &str) -> Result<Option<Pending>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT p.id, p.run, p.card, p.checkpoint, p.created, p.policy_version
                 FROM pending p LEFT JOIN runs r ON r.id = p.run
                 WHERE p.kind = 'approval' AND COALESCE(r.session, 'main') = ?1
                 ORDER BY p.created DESC, p.rowid DESC LIMIT 1",
                params![session],
                pending_row,
            )
            .optional()?)
    }

    /// The label a waiting key request asked with, for that key.
    pub fn asked_secret_label(&self, name: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT json_extract(card, '$.label') FROM pending
                 WHERE kind = 'approval' AND json_extract(card, '$.kind') = 'secret'
                   AND json_extract(card, '$.name') = ?1
                 ORDER BY created DESC LIMIT 1",
                params![name],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Open Action needed cards that are their conversation's latest task's:
    /// (run, conversation), newest first.
    pub fn open_actions(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT r.id, COALESCE(r.session, 'main') FROM runs r
             WHERE r.kind = 'task' AND r.status = 'needs_user'
               AND EXISTS (SELECT 1 FROM pending p WHERE p.kind = 'action' AND p.run = r.id)
               AND NOT EXISTS (SELECT 1 FROM runs n WHERE n.kind = 'task' AND n.rowid > r.rowid
                 AND COALESCE(n.session, 'main') = COALESCE(r.session, 'main')
                 AND n.status NOT IN ('queued', 'skipped'))
             ORDER BY r.rowid DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The latest task (not a consolidation): (id, status, conversation).
    pub fn latest_task(&self) -> Result<Option<(String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, status, COALESCE(session, 'main') FROM runs
                 WHERE kind = 'task' AND status NOT IN ('queued', 'skipped') ORDER BY rowid DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    /// The number of memories eligible for recall.
    pub fn memory_counts(&self) -> Result<i64> {
        self.read(crate::memory::active_count)
    }

    /// The parked approval with this ID.
    pub fn parked(&self, id: &str) -> Result<Option<Pending>> {
        self.pending_where("kind = 'approval' AND id = ?1", Some(id))
    }

    /// The latest parked approval. There can be more than one: a scheduled run
    /// may park while the user's task waits on an approval.
    pub fn any_parked(&self) -> Result<Option<Pending>> {
        self.pending_where("kind = 'approval'", None)
    }

    pub fn add_action(&self, id: &str, run: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pending (id, kind, run) VALUES (?1, 'action', ?2)",
            params![id, run],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn has_action(&self, id: &str) -> Result<bool> {
        Ok(self.action(id)?.is_some())
    }

    /// The Action needed card with this ID.
    pub fn action(&self, id: &str) -> Result<Option<Pending>> {
        self.pending_where("kind = 'action' AND id = ?1", Some(id))
    }

    /// Forgets an approval or action; false if it wasn't there (already answered).
    pub fn remove_pending(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM pending WHERE id = ?1", params![id])? > 0)
    }

    fn pending_where(&self, condition: &str, id: Option<&str>) -> Result<Option<Pending>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT id, run, card, checkpoint, created, policy_version FROM pending WHERE {condition}
                     ORDER BY created DESC LIMIT 1"
                ),
                rusqlite::params_from_iter(id),
                pending_row,
            )
            .optional()?)
    }

    /// Logs an event; its conversation (`data.session`) goes in its own
    /// column too, so a conversation's events can be read on their own.
    pub fn append_event(&self, kind: &str, data: &Value) -> Result<Event> {
        let session = data["session"].as_str().map(String::from);
        let conn = self.conn.lock().unwrap();
        let (seq, ts) = conn.query_row(
            "INSERT INTO events (kind, data, session) VALUES (?1, ?2, ?3) RETURNING seq, ts",
            params![kind, data.to_string(), session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Event {
            seq,
            ts,
            kind: kind.to_string(),
            data: data.clone(),
            session,
        })
    }

    pub fn events_after(&self, after: i64, limit: i64) -> Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![after, limit], event_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Up to `limit` events before `before` (from the newest when None), of
    /// `kinds` only (all when empty), oldest first: the apps load the chat a
    /// page at a time, newest page first. The log's newest seq is read under
    /// the same lock, so the live stream can resume right after it.
    #[cfg(test)]
    pub fn events_page(&self, before: Option<i64>, limit: i64, kinds: &[String]) -> Result<EventPage> {
        self.events_page_in(before, limit, kinds, None)
    }

    /// `events_page`, of one conversation's events and the global ones when
    /// `session` is given.
    pub fn events_page_in(
        &self,
        before: Option<i64>,
        limit: i64,
        kinds: &[String],
        session: Option<&str>,
    ) -> Result<EventPage> {
        let conn = self.conn.lock().unwrap();
        let mut filter = if kinds.is_empty() {
            String::new()
        } else {
            format!(" AND kind IN ({})", vec!["?"; kinds.len()].join(", "))
        };
        if session.is_some() {
            filter.push_str(" AND (session = ? OR session IS NULL)");
        }
        let sql =
            format!("SELECT {EVENT_COLUMNS} FROM events WHERE seq < ?{filter} ORDER BY seq DESC LIMIT ?");
        let mut values: Vec<rusqlite::types::Value> = vec![before.unwrap_or(i64::MAX).into()];
        values.extend(kinds.iter().map(|kind| kind.clone().into()));
        if let Some(session) = session {
            values.push(session.to_string().into());
        }
        values.push((limit + 1).into()); // one more says whether older ones remain
        let mut stmt = conn.prepare(&sql)?;
        let mut events: Vec<Event> = stmt
            .query_map(rusqlite::params_from_iter(values), event_row)?
            .collect::<rusqlite::Result<_>>()?;
        let more = events.len() as i64 > limit;
        events.truncate(limit as usize);
        events.reverse();
        let last_seq = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| row.get(0))?;
        Ok(EventPage {
            events,
            more,
            last_seq,
        })
    }

    pub fn last_seq(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| row.get(0))?)
    }

    /// Adds to `main`'s history.
    #[cfg(test)]
    pub fn add_history(&self, role: &str, text: &str) -> Result<()> {
        self.add_history_in(MAIN, role, text)
    }

    /// Adds to a conversation's history, which counts as its latest change.
    pub fn add_history_in(&self, session: &str, role: &str, text: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO history (role, content, session) VALUES (?1, ?2, ?3)",
            params![role, text, session],
        )?;
        conn.execute(
            "UPDATE sessions SET updated = MAX(updated, ?2) WHERE id = ?1",
            params![session, unix_now()],
        )?;
        Ok(())
    }

    /// `main`'s latest `limit` entries, oldest first.
    #[cfg(test)]
    pub fn history(&self, limit: i64) -> Result<Vec<HistoryEntry>> {
        self.history_in(MAIN, limit)
    }

    /// A conversation's latest `limit` entries, oldest first.
    pub fn history_in(&self, session: &str, limit: i64) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, role, content FROM (SELECT id, ts, role, content FROM history WHERE session = ?2
               ORDER BY id DESC LIMIT ?1)
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![limit, session], |row| {
            Ok(HistoryEntry {
                ts: row.get(0)?,
                role: row.get(1)?,
                text: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT value FROM settings WHERE key = ?1", params![key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn compare_and_set_setting(&self, key: &str, expected: Option<&str>, value: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "INSERT INTO settings (key, value)
             SELECT ?1, ?3 WHERE (SELECT value FROM settings WHERE key = ?1) IS ?2
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, expected, value],
        )?;
        Ok(changed == 1)
    }

    /// Adds a browser's subscription, or updates the one with its endpoint
    /// (new keys after the browser renewed it). Returns its id.
    pub fn save_push_subscription(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        label: &str,
        now: i64,
    ) -> Result<String> {
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, endpoint.as_bytes());
        let id: String = digest.as_ref()[..6].iter().map(|b| format!("{b:02x}")).collect();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO push_subscriptions (id, endpoint, p256dh, auth, label, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(endpoint) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth, label = excluded.label",
            params![id, endpoint, p256dh, auth, label, now],
        )?;
        Ok(id)
    }

    pub fn push_subscriptions(&self) -> Result<Vec<PushSubscription>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, endpoint, p256dh, auth, label, created, last_sent FROM push_subscriptions ORDER BY created",
        )?;
        let rows = stmt.query_map([], push_subscription_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn push_subscription(&self, id: &str) -> Result<Option<PushSubscription>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, endpoint, p256dh, auth, label, created, last_sent FROM push_subscriptions WHERE id = ?1",
                params![id],
                push_subscription_row,
            )
            .optional()?)
    }

    /// Whether there was one to delete.
    pub fn delete_push_subscription(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM push_subscriptions WHERE id = ?1", params![id])? > 0)
    }

    pub fn push_sent(&self, id: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE push_subscriptions SET last_sent = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }

    // --- The iPhone and Mac apps' device tokens (apns.rs) --------------------

    /// Adds an app's device token, or updates the one with the same token
    /// (a renamed device, a new app build). Returns its id.
    pub fn save_device_token(
        &self,
        token: &str,
        topic: &str,
        environment: &str,
        label: &str,
        now: i64,
    ) -> Result<String> {
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, token.as_bytes());
        let id: String = digest.as_ref()[..6].iter().map(|b| format!("{b:02x}")).collect();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO device_tokens (id, token, topic, environment, label, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(token) DO UPDATE SET topic = excluded.topic, environment = excluded.environment, label = excluded.label",
            params![id, token, topic, environment, label, now],
        )?;
        Ok(id)
    }

    pub fn device_tokens(&self) -> Result<Vec<DeviceToken>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, token, topic, environment, label, created, last_sent FROM device_tokens ORDER BY created",
        )?;
        let rows = stmt.query_map([], device_token_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn device_token(&self, id: &str) -> Result<Option<DeviceToken>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, token, topic, environment, label, created, last_sent FROM device_tokens WHERE id = ?1",
                params![id],
                device_token_row,
            )
            .optional()?)
    }

    pub fn delete_device_token(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM device_tokens WHERE id = ?1", params![id])? > 0)
    }

    pub fn device_token_sent(&self, id: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE device_tokens SET last_sent = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }
}

fn pending_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Pending> {
    let card: String = row.get(2)?;
    Ok(Pending {
        id: row.get(0)?,
        run: row.get(1)?,
        card: serde_json::from_str(&card).unwrap_or(Value::Null),
        checkpoint: row.get(3)?,
        created: row.get(4)?,
        policy_version: row.get(5)?,
    })
}

fn device_token_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceToken> {
    Ok(DeviceToken {
        id: row.get(0)?,
        token: row.get(1)?,
        topic: row.get(2)?,
        environment: row.get(3)?,
        label: row.get(4)?,
        created: row.get(5)?,
        last_sent: row.get(6)?,
    })
}

fn push_subscription_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PushSubscription> {
    Ok(PushSubscription {
        id: row.get(0)?,
        endpoint: row.get(1)?,
        p256dh: row.get(2)?,
        auth: row.get(3)?,
        label: row.get(4)?,
        created: row.get(5)?,
        last_sent: row.get(6)?,
    })
}

fn schedule_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: row.get(0)?,
        name: row.get(1)?,
        prompt: row.get(2)?,
        cron: row.get(3)?,
        timezone: row.get(4)?,
        next_run: row.get(5)?,
        last_run: row.get(6)?,
        paused: row.get(7)?,
        created: row.get(8)?,
        policy_version: row.get(9)?,
        created_from: row.get(10)?,
        auto_approve: row.get(11)?,
        session: row.get(12)?,
    })
}

fn secret_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SecretInfo> {
    Ok(SecretInfo {
        name: row.get(0)?,
        label: row.get(1)?,
        created: row.get(2)?,
        updated: row.get(3)?,
        last_used: row.get(4)?,
    })
}

fn memory_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        text: row.get(1)?,
        trusted: row.get(2)?,
        created: row.get(3)?,
    })
}

/// Audit rows that show the user vouched for a memory: their own action, or
/// a v1 confirmation from the event log, copied into the audit trail once
/// (`legacy_confirmed`) by `reconcile_legacy_memory_provenance`.
const USER_PROVENANCE: &str = "(actor = 'user' AND action IN ('create', 'approve', 'update'))
    OR (actor = 'agentd' AND action = 'legacy_confirmed')";

/// Older binaries can still write to the same database after a rollback.
/// Keep their notes, but require a user action in the audit trail for trust:
/// a user-stated memory without one becomes inferred. A v1 confirmation
/// (a `memory.confirmed` event naming a signed-in user) counts as one.
///
/// It runs at every open, so it stays cheap: the audit index settles every
/// memory seen before, and the event log is read once, only when a
/// user-stated memory has no audit evidence yet. A v1 confirmation found
/// there goes into the audit trail, so it's never looked up again.
fn reconcile_legacy_memory_provenance(conn: &Connection) -> Result<()> {
    let unproven = format!(
        "memory.trust = 'user_stated' AND NOT EXISTS (
           SELECT 1 FROM memory_events WHERE memory_id = memory.id AND ({USER_PROVENANCE}))"
    );
    let any: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM memory WHERE {unproven})"),
        [],
        |r| r.get(0),
    )?;
    if !any {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    // One pass over the event log; the earliest confirmation of each memory.
    tx.execute(
        &format!(
            "WITH confirmed AS MATERIALIZED (
               SELECT CASE WHEN json_valid(data) THEN json_extract(data, '$.id') END AS memory_id,
                      CASE WHEN json_valid(data) THEN TRIM(json_extract(data, '$.by')) END AS by_user,
                      CAST(strftime('%s', ts) AS INTEGER) AS at, seq
               FROM events WHERE kind = 'memory.confirmed')
             INSERT INTO memory_events (memory_id, actor, action, by_user, created)
             SELECT id, 'agentd', 'legacy_confirmed', by_user, at FROM (
               SELECT memory.id AS id, confirmed.by_user AS by_user, COALESCE(confirmed.at, ?1) AS at,
                      MIN(confirmed.seq)
               FROM memory JOIN confirmed ON confirmed.memory_id = memory.id
               WHERE {unproven} AND LENGTH(confirmed.by_user) > 0
               GROUP BY memory.id)"
        ),
        params![unix_now()],
    )?;
    tx.execute(
        &format!("UPDATE memory SET trust = 'agent_inferred', version = version + 1 WHERE {unproven}"),
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// Schema changes to databases made by earlier versions (PRAGMA user_version).
/// Every change is additive, so an older agentd still reads and writes a
/// newer database (it ignores versions above its own).
fn migrate(conn: &Connection) -> Result<()> {
    migrate_to(conn, LATEST_VERSION)
}

/// The schema version this agentd migrates to.
const LATEST_VERSION: i64 = 9;

/// Migrates up to `target` (tests build older databases with it).
fn migrate_to(conn: &Connection, target: i64) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 && target >= 1 {
        // Whose thread a history entry belongs to: "main" until there are bots.
        if conn.prepare("SELECT agent FROM history LIMIT 0").is_err() {
            conn.execute_batch("ALTER TABLE history ADD COLUMN agent TEXT NOT NULL DEFAULT 'main'")?;
        }
        conn.execute_batch("PRAGMA user_version = 1")?;
    }
    if version < 2 && target >= 2 {
        // One transaction: a failed step leaves the database at version 1.
        conn.execute_batch(&format!("BEGIN; {MIGRATION_2} COMMIT;"))?;
    }
    if version < 3 && target >= 3 {
        // Whether a task changed something (a command, a write in the browser,
        // a schedule): only those get a work-log entry. Null for older tasks.
        conn.execute_batch(
            "BEGIN; ALTER TABLE runs ADD COLUMN acted INTEGER; PRAGMA user_version = 3; COMMIT;",
        )?;
    }
    if version < 4 && target >= 4 {
        // "Run without asking", per schedule: off unless a person turns it on.
        conn.execute_batch(
            "BEGIN; ALTER TABLE schedules ADD COLUMN auto_approve INTEGER NOT NULL DEFAULT 0; \
             PRAGMA user_version = 4; COMMIT;",
        )?;
    }
    if version < 5 && target >= 5 {
        // The scheduled runs' queue (schedule.rs): when a queued run is due,
        // and which try it is.
        conn.execute_batch(
            "BEGIN; ALTER TABLE runs ADD COLUMN due INTEGER; \
             ALTER TABLE runs ADD COLUMN attempt INTEGER NOT NULL DEFAULT 0; \
             CREATE INDEX IF NOT EXISTS runs_by_status ON runs (status); \
             PRAGMA user_version = 5; COMMIT;",
        )?;
    }
    if version < 6 && target >= 6 {
        // Sites the user allowed the shell to reach (access.rs).
        conn.execute_batch(
            "BEGIN; CREATE TABLE IF NOT EXISTS egress_hosts (
                 host TEXT PRIMARY KEY,
                 added INTEGER NOT NULL,
                 by_user TEXT,
                 task TEXT,
                 why TEXT);
             PRAGMA user_version = 6; COMMIT;",
        )?;
    }
    if version < 7 && target >= 7 {
        // The iPhone and Mac apps to notify by APNs (apns.rs).
        conn.execute_batch(
            "BEGIN; CREATE TABLE IF NOT EXISTS device_tokens (
                 id TEXT PRIMARY KEY,
                 token TEXT NOT NULL UNIQUE,
                 topic TEXT NOT NULL,
                 environment TEXT NOT NULL DEFAULT 'production',
                 label TEXT NOT NULL DEFAULT '',
                 created INTEGER NOT NULL,
                 last_sent INTEGER);
             PRAGMA user_version = 7; COMMIT;",
        )?;
    }
    if version < 8 && target >= 8 {
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE runs ADD COLUMN memory_consolidated INTEGER NOT NULL DEFAULT 0;
             UPDATE runs SET memory_consolidated = 1
               WHERE kind = 'task' AND schedule IS NULL AND rowid <= COALESCE((
                 SELECT CAST(value AS INTEGER) FROM settings WHERE key = 'memory_last_consolidated_run'
               ), 0);
             CREATE INDEX IF NOT EXISTS runs_pending_consolidation ON runs(memory_consolidated)
               WHERE kind = 'task' AND agent = 'main' AND schedule IS NULL;
             UPDATE events SET data = json_remove(data, '$.text')
               WHERE kind = 'memory.saved' AND json_valid(data);
             UPDATE events SET data = json_remove(data, '$.args')
               WHERE kind = 'step' AND json_valid(data)
                 AND json_extract(data, '$.tool') IN ('remember', 'forget', 'search_memory');
             UPDATE pending SET checkpoint = json_set(json_remove(checkpoint, '$.memory_block'),
               '$.messages[0].content', '') WHERE json_valid(checkpoint);
             PRAGMA user_version = 8;
             COMMIT;",
        )?;
    }
    if version < 9 && target >= 9 {
        // Conversations, queued messages and usage per run (0.16.0). One
        // transaction: a failed step leaves the database at version 8.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(MIGRATION_9)?;
        // `main` holds everything from before, titled from its first message.
        let first: Option<String> = tx
            .query_row(
                "SELECT content FROM history WHERE role = 'user' ORDER BY id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let title = first
            .as_deref()
            .map(crate::sessions::title_from)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "First conversation".into());
        let (created, updated): (Option<i64>, Option<i64>) = tx.query_row(
            "SELECT CAST(strftime('%s', MIN(ts)) AS INTEGER), CAST(strftime('%s', MAX(ts)) AS INTEGER) FROM history",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let now = unix_now();
        tx.execute(
            "INSERT OR IGNORE INTO sessions (id, title, created, updated) VALUES ('main', ?1, ?2, ?3)",
            params![title, created.unwrap_or(now), updated.or(created).unwrap_or(now)],
        )?;
        tx.execute_batch("PRAGMA user_version = 9")?;
        tx.commit()?;
    }
    Ok(())
}

/// Conversations (`sessions`; `history`, `runs`, `schedules` and `events`
/// gain a `session`), queued messages (`chat_queue`), and each run's tokens.
/// Additive: an older agentd writes rows these columns default for.
///
/// The event log's conversation events are tagged `main`: the chat's kinds,
/// and anything about a task, but never the global `access.*` ones.
const MIGRATION_9: &str = "
    CREATE TABLE IF NOT EXISTS sessions (
      id TEXT PRIMARY KEY, agent TEXT NOT NULL DEFAULT 'main',
      title TEXT NOT NULL DEFAULT '', created INTEGER NOT NULL, updated INTEGER NOT NULL,
      archived INTEGER NOT NULL DEFAULT 0, deleted INTEGER, tab_target TEXT);
    ALTER TABLE history ADD COLUMN session TEXT NOT NULL DEFAULT 'main';
    CREATE INDEX IF NOT EXISTS history_by_session ON history (session, id);
    ALTER TABLE runs ADD COLUMN session TEXT;
    ALTER TABLE runs ADD COLUMN input_tokens INTEGER;
    ALTER TABLE runs ADD COLUMN output_tokens INTEGER;
    ALTER TABLE runs ADD COLUMN cached_tokens INTEGER;
    UPDATE runs SET session = 'main' WHERE kind = 'task';
    CREATE INDEX IF NOT EXISTS runs_by_started ON runs (started);
    ALTER TABLE schedules ADD COLUMN session TEXT NOT NULL DEFAULT 'main';
    ALTER TABLE events ADD COLUMN session TEXT;
    UPDATE events SET session = 'main'
      WHERE kind NOT LIKE 'access.%' AND (
        kind IN ('message', 'step', 'plan', 'review', 'shell', 'shell.output', 'action_needed',
                 'action.resolved', 'image')
        OR kind LIKE 'task.%' OR kind LIKE 'run.%' OR kind LIKE 'approval.%'
        OR (json_valid(data) AND json_extract(data, '$.task') IS NOT NULL));
    CREATE INDEX IF NOT EXISTS events_by_session ON events (session, seq);
    CREATE TABLE IF NOT EXISTS chat_queue (
      id TEXT PRIMARY KEY, session TEXT NOT NULL, text TEXT NOT NULL,
      by_user TEXT, created INTEGER NOT NULL);
";

/// Memory v2 (kinds, trust, status, history, full-text search), the effects
/// queue, and policy versions on parked work (docs/memory-v2.md §4).
///
/// Ids keep the types the tables already have: memory and schedule ids are
/// integers, run ids text. `created` on memory is an ISO timestamp; the new
/// time columns are unix seconds.
///
/// `trusted` stays, derived from `trust` and `status`, so 0.14.0 can still read
/// the table after a rollback; the `memory_legacy_*` triggers keep `trust` in
/// step when code that only knows `trusted` writes (0.14.0, or the v1 path
/// with MEMORY_V2 off).
const MIGRATION_2: &str = "
    ALTER TABLE memory ADD COLUMN kind TEXT NOT NULL DEFAULT 'fact';
    ALTER TABLE memory ADD COLUMN status TEXT NOT NULL DEFAULT 'active';
    ALTER TABLE memory ADD COLUMN trust TEXT NOT NULL DEFAULT 'agent_inferred';
    ALTER TABLE memory ADD COLUMN importance REAL NOT NULL DEFAULT 0.5;
    ALTER TABLE memory ADD COLUMN source_run TEXT;
    ALTER TABLE memory ADD COLUMN source_seq INTEGER;
    ALTER TABLE memory ADD COLUMN source_schedule INTEGER;
    ALTER TABLE memory ADD COLUMN valid_until INTEGER;
    ALTER TABLE memory ADD COLUMN superseded_by INTEGER;
    ALTER TABLE memory ADD COLUMN pending_reason TEXT;
    ALTER TABLE memory ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE memory ADD COLUMN updated INTEGER;
    ALTER TABLE memory ADD COLUMN last_recalled INTEGER;
    ALTER TABLE memory ADD COLUMN recall_count INTEGER NOT NULL DEFAULT 0;
    UPDATE memory SET trust = CASE WHEN trusted = 1 THEN 'user_stated' ELSE 'agent_inferred' END,
                      updated = CAST(strftime('%s', created) AS INTEGER);
    CREATE INDEX IF NOT EXISTS memory_lookup ON memory(agent, kind, status);
    CREATE INDEX IF NOT EXISTS memory_expiry ON memory(valid_until) WHERE valid_until IS NOT NULL;
    CREATE UNIQUE INDEX IF NOT EXISTS memory_worklog_run ON memory(source_run) WHERE kind = 'work_log';

    CREATE TRIGGER IF NOT EXISTS memory_legacy_insert AFTER INSERT ON memory
      WHEN new.trusted = 1 AND new.trust <> 'user_stated' BEGIN
        UPDATE memory SET trust = 'user_stated' WHERE id = new.id; END;
    CREATE TRIGGER IF NOT EXISTS memory_legacy_confirm AFTER UPDATE OF trusted ON memory
      WHEN new.trusted = 1 AND old.trusted = 0 BEGIN
        UPDATE memory SET trust = 'user_stated', status = 'active' WHERE id = new.id; END;
    CREATE TRIGGER IF NOT EXISTS memory_trusted AFTER UPDATE OF trust, status ON memory BEGIN
        UPDATE memory SET trusted = (new.trust = 'user_stated' AND new.status = 'active')
        WHERE id = new.id AND trusted <> (new.trust = 'user_stated' AND new.status = 'active'); END;

    CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(id UNINDEXED, text, tokenize = 'porter unicode61');
    CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory BEGIN
      INSERT INTO memory_fts(id, text) VALUES (new.id, new.text); END;
    CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE OF text ON memory BEGIN
      DELETE FROM memory_fts WHERE id = old.id; INSERT INTO memory_fts(id, text) VALUES (new.id, new.text); END;
    CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory BEGIN
      DELETE FROM memory_fts WHERE id = old.id; END;
    INSERT INTO memory_fts(id, text) SELECT id, text FROM memory;

    CREATE TABLE IF NOT EXISTS memory_events (
      seq        INTEGER PRIMARY KEY AUTOINCREMENT,
      memory_id  INTEGER NOT NULL,
      actor      TEXT NOT NULL,
      action     TEXT NOT NULL,
      by_user    TEXT,
      run        TEXT,
      before     TEXT,
      after      TEXT,
      created    INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS memory_events_memory ON memory_events(memory_id, seq);

    CREATE TABLE IF NOT EXISTS effects (
      kind       TEXT NOT NULL,
      key        TEXT NOT NULL,
      payload    TEXT NOT NULL,
      due        INTEGER NOT NULL,
      attempts   INTEGER NOT NULL DEFAULT 0,
      last_error TEXT,
      created    INTEGER NOT NULL,
      PRIMARY KEY (kind, key));
    CREATE INDEX IF NOT EXISTS effects_due ON effects(due);

    ALTER TABLE pending   ADD COLUMN policy_version TEXT;
    ALTER TABLE schedules ADD COLUMN policy_version TEXT;
    ALTER TABLE schedules ADD COLUMN created_from TEXT NOT NULL DEFAULT 'user';
    ALTER TABLE runs      ADD COLUMN kind TEXT NOT NULL DEFAULT 'task';
    ALTER TABLE runs      ADD COLUMN policy_version TEXT;
    ALTER TABLE runs      ADD COLUMN schedule INTEGER;

    PRAGMA user_version = 2;
";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_history_and_settings() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let first = store
            .append_event("message", &serde_json::json!({"text": "hi"}))
            .unwrap();
        let second = store
            .append_event("step", &serde_json::json!({"tool": "read_text"}))
            .unwrap();
        assert!(second.seq > first.seq);
        assert_eq!(store.events_after(first.seq, 10).unwrap()[0].kind, "step");
        assert_eq!(store.last_seq().unwrap(), second.seq);
        let page = store.events_page(None, 10, &[]).unwrap();
        assert_eq!(
            (page.events.len(), page.more, page.last_seq),
            (2, false, second.seq)
        );
        for i in 0..5 {
            store.add_history("user", &format!("m{i}")).unwrap();
        }
        let last_two: Vec<_> = store.history(2).unwrap().into_iter().map(|h| h.text).collect();
        assert_eq!(last_two, ["m3", "m4"]);
        assert_eq!(store.setting("model").unwrap(), None);
        store.set_setting("model", "a").unwrap();
        store.set_setting("model", "b").unwrap();
        assert_eq!(store.setting("model").unwrap().as_deref(), Some("b"));
    }

    #[test]
    fn the_chat_comes_a_page_at_a_time_newest_first() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut messages = vec![];
        for i in 0..5 {
            messages.push(store.append_event("message", &json!({"text": i})).unwrap().seq);
            store.append_event("step", &json!({"tool": "read_text"})).unwrap();
        }
        let last = store
            .append_event("shell.output", &json!({"output": "…"}))
            .unwrap();
        let kinds = vec!["message".to_string()];

        let newest = store.events_page(None, 2, &kinds).unwrap();
        let seqs: Vec<_> = newest.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, messages[3..]); // oldest first, steps left out
        assert!(newest.more);
        assert_eq!(newest.last_seq, last.seq); // the stream resumes after the step and the output

        let older = store.events_page(Some(seqs[0]), 2, &kinds).unwrap();
        assert_eq!(
            older.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            messages[1..3]
        );
        assert!(older.more);
        let oldest = store.events_page(Some(messages[1]), 2, &kinds).unwrap();
        assert_eq!(oldest.events.len(), 1);
        assert!(!oldest.more);

        // No kinds: everything, steps included.
        assert_eq!(store.events_page(None, 100, &[]).unwrap().events.len(), 11);
    }

    #[test]
    fn runs_park_and_resume() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .start_run(
                "r1",
                "do it",
                Some("me@example.com"),
                "m",
                "0123456789abcdef",
                None,
            )
            .unwrap();
        assert_eq!(store.runs_with_status("running").unwrap(), ["r1"]);
        let card = serde_json::json!({"kind": "shell", "command": "sudo apt update"});
        store
            .park("a1", "r1", &card, "{\"step\":3}", "0123456789abcdef")
            .unwrap();
        store.set_run_status("r1", "waiting").unwrap();
        assert!(store.runs_with_status("running").unwrap().is_empty());
        let parked = store.parked("a1").unwrap().unwrap();
        assert_eq!(
            (parked.run.as_str(), parked.card["command"].as_str()),
            ("r1", Some("sudo apt update"))
        );
        assert_eq!(store.any_parked().unwrap().unwrap().id, "a1");
        // Actions aren't approvals.
        store.add_action("x1", "r1").unwrap();
        assert!(store.has_action("x1").unwrap());
        assert!(store.parked("x1").unwrap().is_none());
        assert!(store.remove_pending("a1").unwrap());
        assert!(!store.remove_pending("a1").unwrap());
        assert!(store.any_parked().unwrap().is_none());
        store.finish_run("r1", "done", 4, Some(0.01), "ok").unwrap();
        assert_eq!(store.runs_with_status("done").unwrap(), ["r1"]);
    }

    #[test]
    fn schedules_come_due_move_on_and_pause() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let daily = store
            .add_schedule(
                "Digest",
                "Summarize my inbox",
                Some("0 8 * * *"),
                "America/Toronto",
                1_000,
            )
            .unwrap();
        let once = store
            .add_schedule("Check PR", "Check PR 12", None, "America/Toronto", 2_000)
            .unwrap();
        assert_eq!((daily.cron.as_deref(), daily.paused), (Some("0 8 * * *"), false));
        assert_eq!(store.next_due().unwrap(), Some(1_000));
        assert!(store.due_schedules(999).unwrap().is_empty());
        let due: Vec<i64> = store.due_schedules(5_000).unwrap().iter().map(|s| s.id).collect();
        assert_eq!(due, [daily.id, once.id]); // oldest first
        store.schedule_ran(daily.id, Some(1_001), 90_000).unwrap();
        let moved = store.schedule(daily.id).unwrap().unwrap();
        assert_eq!((moved.next_run, moved.last_run), (90_000, Some(1_001)));
        // Skipped (not run): last_run stays.
        store.schedule_ran(daily.id, None, 180_000).unwrap();
        assert_eq!(store.schedule(daily.id).unwrap().unwrap().last_run, Some(1_001));
        assert!(store.set_schedule_paused(once.id, true, None).unwrap());
        assert!(store.due_schedules(5_000).unwrap().is_empty());
        assert_eq!(store.next_due().unwrap(), Some(180_000));
        assert!(store.delete_schedule(once.id).unwrap());
        assert!(!store.delete_schedule(once.id).unwrap());
        assert_eq!(store.schedules().unwrap().len(), 1);
    }

    #[test]
    fn remembers_forgets_and_confirms() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let a = store.remember("The user's manager is Lawrence", true).unwrap();
        let b = store
            .remember("Invoices go to billing@example.com", false)
            .unwrap();
        assert!(a.trusted && !b.trusted);
        assert_eq!(
            store
                .memories(10)
                .unwrap()
                .iter()
                .map(|m| m.id)
                .collect::<Vec<_>>(),
            [a.id, b.id]
        );
        assert!(store.confirm_memory(b.id).unwrap());
        assert!(store.memories(10).unwrap()[1].trusted);
        assert!(store.forget(a.id).unwrap());
        assert!(!store.forget(a.id).unwrap());
        assert_eq!(store.memories(1).unwrap().len(), 1);
    }

    #[test]
    fn saved_keys_keep_their_first_date_and_record_use() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let first = store.save_secret("GITHUB_TOKEN", "GitHub token").unwrap();
        assert_eq!(first.last_used, None);
        let replaced = store.save_secret("GITHUB_TOKEN", "GitHub token (acme)").unwrap();
        assert_eq!(
            (replaced.label.as_str(), &replaced.created),
            ("GitHub token (acme)", &first.created)
        );
        store.secrets_used(&["GITHUB_TOKEN".into()]).unwrap();
        let listed = store.secrets().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].last_used.is_some());
        assert!(store.delete_secret("GITHUB_TOKEN").unwrap());
        assert!(!store.delete_secret("GITHUB_TOKEN").unwrap());
    }

    /// agentd 0.14.0's database (schema version 1), opened by this version.
    fn migrated_v1() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("agentd-v1-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agentd.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(include_str!("../testdata/agentd-v1.sql"))
            .unwrap();
        (Store::open(&path).unwrap(), dir)
    }

    fn memory_rows(store: &Store) -> Vec<(i64, String, bool, String, String, String)> {
        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, text, trusted, trust, status, kind FROM memory ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn fts(store: &Store, query: &str) -> Vec<i64> {
        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM memory_fts WHERE memory_fts MATCH ?1 ORDER BY id")
            .unwrap();
        stmt.query_map(params![query], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn the_run_queue_keeps_one_per_schedule_in_due_order() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let q = |id: &str, schedule: i64, due: i64| {
            store.enqueue_run(id, schedule, "p", "m", "v", due, 0).unwrap()
        };
        assert!(q("a", 1, 200));
        assert!(!q("a2", 1, 300)); // schedule 1 has one queued already
        assert!(q("b", 2, 100));
        assert_eq!(store.queued_count().unwrap(), 2);
        assert_eq!(store.next_queued().unwrap(), Some(100));
        assert!(store.claim_run(50).unwrap().is_none()); // none due yet
        let first = store.claim_run(1000).unwrap().unwrap();
        assert_eq!((first.id.as_str(), first.schedule, first.due), ("b", 2, 100));
        assert!(!q("b2", 2, 400)); // still running
        assert_eq!(
            store.running_scheduled_runs().unwrap(),
            std::slice::from_ref(&first)
        );
        assert_eq!(store.cancel_queued_runs(1).unwrap(), 1);
        assert!(store.claim_run(1000).unwrap().is_none());
        store.finish_run("b", "done", 2, None, "ok").unwrap();
        assert!(q("b3", 2, 500)); // free again
    }

    #[test]
    fn migrates_a_v1_database_to_the_latest() {
        let (store, dir) = migrated_v1();
        {
            let conn = store.conn.lock().unwrap();
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(version, 9);
            // Everything from before is in the first conversation.
            let main: String = conn
                .query_row("SELECT id FROM sessions", [], |r| r.get(0))
                .unwrap();
            assert_eq!(main, "main");
            // The apps' device-token table remains available.
            assert!(
                conn.prepare("SELECT token, topic FROM device_tokens LIMIT 0")
                    .is_ok()
            );
            // Older runs: no due time, first try.
            let attempt: Option<i64> = conn
                .query_row("SELECT attempt FROM runs LIMIT 1", [], |r| r.get(0))
                .optional()
                .unwrap();
            assert!(attempt.is_none_or(|a| a == 0));
            // Schedules from before don't run without asking.
            let auto: Option<bool> = conn
                .query_row("SELECT auto_approve FROM schedules LIMIT 1", [], |r| r.get(0))
                .optional()
                .unwrap();
            assert_ne!(auto, Some(true));
            // Tasks from before don't say whether they changed anything.
            let acted: Option<i64> = conn
                .query_row("SELECT acted FROM runs LIMIT 1", [], |r| r.get(0))
                .optional()
                .unwrap()
                .flatten();
            assert_eq!(acted, None);
            // Legacy memories remain active facts. They have no user audit provenance.
            let updated: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memory WHERE updated > 1700000000",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(updated, 3);
            // Parked work and schedules from before have no policy version yet.
            let (policy, kind): (Option<String>, String) = conn
                .query_row(
                    "SELECT p.policy_version, r.kind FROM pending p JOIN runs r ON r.id = p.run",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!((policy, kind.as_str()), (None, "task"));
            let from: String = conn
                .query_row("SELECT created_from FROM schedules", [], |r| r.get(0))
                .unwrap();
            assert_eq!(from, "user");
        }
        let rows = memory_rows(&store);
        let summary: Vec<(bool, &str, &str, &str)> = rows
            .iter()
            .map(|r| (r.2, r.3.as_str(), r.4.as_str(), r.5.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                (false, "agent_inferred", "active", "fact"),
                (false, "agent_inferred", "active", "fact"),
                (false, "agent_inferred", "active", "fact"),
            ]
        );
        // The full-text index holds the existing memories.
        assert_eq!(fts(&store, "notion"), [2]);
        assert_eq!(fts(&store, "summaries"), [1]);
        // 0.14.0's reads still work, and opening again doesn't migrate again.
        assert_eq!(store.memories(60).unwrap().len(), 3);
        assert_eq!(store.runs_with_status("waiting").unwrap(), ["b2c3d4e5f6a1"]);
        assert!(store.parked("c3d4e5f6a1b2").unwrap().is_some());
        assert_eq!(store.schedules().unwrap().len(), 1);
        drop(store);
        let reopened = Store::open(&dir.join("agentd.db")).unwrap();
        assert_eq!(memory_rows(&reopened).len(), 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trusted_and_trust_stay_in_step_for_old_and_new_writers() {
        let (store, dir) = migrated_v1();
        // 0.14.0's statements (also the v1 path with MEMORY_V2 off).
        let trusted = store.remember("Reply in English", true).unwrap();
        let hint = store.remember("The office is on floor 3", false).unwrap();
        let rows = memory_rows(&store);
        let row = |id: i64| rows.iter().find(|r| r.0 == id).unwrap().clone();
        assert_eq!(row(trusted.id).3, "user_stated");
        assert_eq!(row(hint.id).3, "agent_inferred");
        store.confirm_memory(hint.id).unwrap();
        let confirmed = memory_rows(&store).into_iter().find(|r| r.0 == hint.id).unwrap();
        assert_eq!((confirmed.2, confirmed.3.as_str()), (true, "user_stated"));
        // v2 writers set trust and status; `trusted` follows for 0.14.0.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE memory SET status = 'deleted' WHERE id = ?1",
                params![trusted.id],
            )
            .unwrap();
            conn.execute(
                "UPDATE memory SET status = 'pending', trust = 'agent_inferred' WHERE id = ?1",
                params![hint.id],
            )
            .unwrap();
        }
        let after: Vec<(i64, bool)> = memory_rows(&store).iter().map(|r| (r.0, r.2)).collect();
        assert!(after.contains(&(trusted.id, false)) && after.contains(&(hint.id, false)));
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE memory SET status = 'active', trust = 'user_stated' WHERE id = ?1",
                params![hint.id],
            )
            .unwrap();
        }
        assert!(memory_rows(&store).iter().any(|r| r.0 == hint.id && r.2));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The reconciliation as it was before it kept v1 confirmations in the
    /// audit trail: one scan of the event log per memory, at every open.
    const PER_ROW_RECONCILE: &str = "UPDATE memory SET trust = 'agent_inferred', version = version + 1
         WHERE trust = 'user_stated'
           AND NOT EXISTS (
             SELECT 1 FROM memory_events WHERE memory_id = memory.id AND actor = 'user'
               AND action IN ('create', 'approve', 'update')
           )
           AND NOT EXISTS (
             SELECT 1 FROM events WHERE kind = 'memory.confirmed'
               AND CASE WHEN json_valid(data) THEN json_extract(data, '$.id') END = memory.id
               AND CASE WHEN json_valid(data) THEN LENGTH(TRIM(json_extract(data, '$.by'))) END > 0
           )";

    /// User-stated memories as an earlier agentd left them, with and without
    /// evidence of a user action.
    fn seed_provenance(conn: &Connection) {
        conn.execute_batch(
            r#"INSERT INTO memory (id, text, trusted, trust) VALUES
                 (1, 'Said in the app', 1, 'user_stated'),
                 (2, 'Confirmed in v1', 1, 'user_stated'),
                 (3, 'Confirmed without a login', 1, 'user_stated'),
                 (4, 'Saved by a v1 task', 1, 'user_stated'),
                 (5, 'Saved by a v2 task', 1, 'user_stated'),
                 (6, 'Confirmed, id as text', 1, 'user_stated'),
                 (7, 'Confirmed with a null login', 1, 'user_stated'),
                 (8, 'A hint', 0, 'agent_inferred');
               INSERT INTO memory_events (memory_id, actor, action, created) VALUES
                 (1, 'user', 'create', 1), (5, 'agent', 'create', 1), (8, 'user', 'forget', 1);
               INSERT INTO events (kind, data) VALUES
                 ('memory.confirmed', '{"id": 2, "by": "me@example.com"}'),
                 ('memory.confirmed', '{"id": 2, "by": "later@example.com"}'),
                 ('memory.confirmed', '{"id": 3, "by": "  "}'),
                 ('memory.confirmed', '{"id": "6", "by": "me@example.com"}'),
                 ('memory.confirmed', '{"id": 7, "by": null}'),
                 ('memory.confirmed', 'not json'),
                 ('memory.saved', '{"id": 4, "by": "me@example.com"}');"#,
        )
        .unwrap();
    }

    fn trusts(store: &Store) -> Vec<(i64, String, bool, i64)> {
        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, trust, trusted, version FROM memory ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn provenance_reconciles_like_the_per_row_check_and_reads_the_event_log_once() {
        let dir = std::env::temp_dir().join(format!("agentd-prov-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agentd.db");
        seed_provenance(&Store::open(&path).unwrap().conn.lock().unwrap());
        let store = Store::open(&path).unwrap();
        // The same data under the per-row check.
        let before = Store::open(Path::new(":memory:")).unwrap();
        {
            let conn = before.conn.lock().unwrap();
            seed_provenance(&conn);
            conn.execute(PER_ROW_RECONCILE, []).unwrap();
        }
        let reconciled = trusts(&store);
        assert_eq!(reconciled, trusts(&before));
        let user_stated: Vec<i64> = reconciled
            .iter()
            .filter(|(_, trust, trusted, _)| trust == "user_stated" && *trusted)
            .map(|row| row.0)
            .collect();
        assert_eq!(user_stated, [1, 2, 6]);
        // The v1 confirmations are in the audit trail now, the earliest one's login with each.
        let legacy = |store: &Store| -> Vec<(i64, Option<String>)> {
            let conn = store.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT memory_id, by_user FROM memory_events WHERE action = 'legacy_confirmed' ORDER BY memory_id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            legacy(&store),
            [
                (2, Some("me@example.com".into())),
                (6, Some("me@example.com".into()))
            ]
        );
        // Later opens settle everything from the audit trail: no event log
        // needed, nothing recorded twice, no version churn.
        store
            .conn
            .lock()
            .unwrap()
            .execute("DELETE FROM events", [])
            .unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(trusts(&reopened), reconciled);
        assert_eq!(legacy(&reopened).len(), 2);
        // A memory an older binary marks user-stated after a rollback is still caught.
        reopened
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO memory (id, text, trusted, trust) VALUES (9, 'From a rollback', 1, 'user_stated')",
                [],
            )
            .unwrap();
        drop(reopened);
        let again = Store::open(&path).unwrap();
        assert_eq!(
            trusts(&again).last().map(|row| row.1.clone()),
            Some("agent_inferred".into())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn full_text_search_follows_the_memory_table() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let a = store
            .remember("Invoices go to billing@example.com", true)
            .unwrap();
        let b = store
            .remember("The staging server is called kestrel", false)
            .unwrap();
        assert_eq!(fts(&store, "invoice"), [a.id]); // porter stemming
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE memory SET text = 'The staging server is called osprey' WHERE id = ?1",
                params![b.id],
            )
            .unwrap();
        }
        assert!(fts(&store, "kestrel").is_empty());
        assert_eq!(fts(&store, "osprey"), [b.id]);
        store.forget(a.id).unwrap();
        assert!(fts(&store, "invoice").is_empty());
    }

    #[test]
    fn effects_are_replaced_by_key_and_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("agentd-fx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agentd.db");
        let store = Store::open(&path).unwrap();
        store
            .enqueue_effect("consolidate", "main", &json!({"since": "r1"}), 100)
            .unwrap();
        let first = store.due_effects(1_000, 10).unwrap().remove(0);
        store
            .effect_again(&first, 150, Some("the model answered 503"))
            .unwrap();
        // A later trigger replaces payload and due, and keeps the attempts.
        store
            .enqueue_effect("consolidate", "main", &json!({"since": "r2"}), 500)
            .unwrap();
        let replaced = store.due_effects(1_000, 10).unwrap();
        assert_eq!(replaced.len(), 1);
        assert_eq!(
            (replaced[0].payload.clone(), replaced[0].due, replaced[0].attempts),
            (json!({"since": "r2"}), 500, 1)
        );
        assert!(store.due_effects(499, 10).unwrap().is_empty());
        assert_eq!(store.next_effect_due().unwrap(), Some(500));
        // Finishing the old run doesn't remove the new trigger.
        store.effect_done(&first).unwrap();
        assert_eq!(store.due_effects(1_000, 10).unwrap().len(), 1);
        drop(store);
        let reopened = Store::open(&path).unwrap();
        let after = reopened.due_effects(1_000, 10).unwrap();
        assert_eq!(after, replaced);
        reopened.effect_done(&after[0]).unwrap();
        assert_eq!(reopened.effects_health().unwrap(), (0, vec![]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_purge_removes_only_long_deleted_memories() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let old = store.remember("Old and deleted", true).unwrap();
        let recent = store.remember("Recently deleted", true).unwrap();
        let kept = store.remember("Still active", true).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE memory SET status = 'deleted', updated = 1000 WHERE id = ?1",
                params![old.id],
            )
            .unwrap();
            conn.execute(
                "UPDATE memory SET status = 'deleted', updated = 5000 WHERE id = ?1",
                params![recent.id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO memory_events (memory_id, actor, action, before, after, created)
                 VALUES (?1, 'user', 'forget', 'Old and deleted', NULL, 1000)",
                params![old.id],
            )
            .unwrap();
        }
        assert_eq!(store.purge_deleted_memories(2000).unwrap(), 1);
        let ids: Vec<i64> = memory_rows(&store).iter().map(|r| r.0).collect();
        assert_eq!(ids, [recent.id, kept.id]);
        assert!(fts(&store, "old").is_empty());
        let conn = store.conn.lock().unwrap();
        let history: Vec<(String, Option<String>)> = conn
            .prepare("SELECT action, before FROM memory_events WHERE memory_id = ?1 ORDER BY seq")
            .unwrap()
            .query_map(params![old.id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(history, [("forget".into(), None), ("purge".into(), None)]);
    }

    /// agentd 0.15's database (schema version 8) with a chat, a parked card,
    /// a scheduled run and the event log's kinds, opened by this version.
    #[test]
    fn migrates_a_v8_database_and_an_older_agentd_still_writes_to_it() {
        let dir = std::env::temp_dir().join(format!("agentd-v8-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agentd.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(include_str!("../testdata/agentd-v1.sql"))
                .unwrap();
            migrate_to(&conn, 8).unwrap();
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(version, 8);
            conn.execute_batch(
                r#"INSERT INTO runs (id, status, request, model, schedule, acted, cost)
                     VALUES ('sched1', 'done', 'Check the build', 'm', 1, 0, 0.01);
                   INSERT INTO runs (id, kind, status, request, model, cost)
                     VALUES ('tidy1', 'consolidation', 'done', 'consolidate 2 tasks', 'm', 0.001);
                   INSERT INTO events (kind, data) VALUES
                     ('message', '{"role":"assistant","text":"A note."}'),
                     ('step', '{"task":"a1b2c3d4e5f6","step":1,"tool":"read_text"}'),
                     ('task.stopping', '{}'),
                     ('approval.requested', '{"id":"c3d4e5f6a1b2","kind":"shell","task":"b2c3d4e5f6a1"}'),
                     ('run.finished', '{"task":"sched1","status":"done"}'),
                     ('memory.saved', '{"id":4,"task":"a1b2c3d4e5f6"}'),
                     ('memory.forgotten', '{"id":4,"by":"me@example.com"}'),
                     ('access.allowed', '{"hosts":["a.example.com"],"task":"b2c3d4e5f6a1"}'),
                     ('settings', '{"model":"m"}'),
                     ('secret.saved', '{"name":"GITHUB_TOKEN"}'),
                     ('schedule.updated', '{"id":1,"next_run":5}'),
                     ('not.json', 'oops');"#,
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        // `main` holds everything from before, titled from its first message.
        let main = store.session(MAIN).unwrap().unwrap();
        assert_eq!(main.title, "Remember that I like summaries short");
        assert_eq!(main.created, 1_793_554_671); // 2026-11-01T17:37:51Z, its first entry
        assert!(!main.archived && main.deleted.is_none());
        assert_eq!(store.history_in(MAIN, 10).unwrap().len(), 2);
        assert_eq!(
            store.run_session("a1b2c3d4e5f6").unwrap().as_deref(),
            Some("main")
        );
        assert_eq!(store.run_session("sched1").unwrap().as_deref(), Some("main"));
        assert_eq!(store.run_session("tidy1").unwrap(), None);
        assert_eq!(store.schedule(1).unwrap().unwrap().session, "main");
        assert_eq!(store.parked_in(MAIN).unwrap().unwrap().id, "c3d4e5f6a1b2");
        assert!(store.queued_in(MAIN).unwrap().is_empty());
        // The event log: the chat's kinds and anything about a task are
        // main's; the global ones stay without.
        let tagged: Vec<(String, Option<String>)> = store
            .events_after(0, 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.session))
            .collect();
        let main_ = Some("main".to_string());
        assert_eq!(
            tagged,
            [
                ("memory.saved".into(), None), // the fixture's, from the app
                ("message".into(), main_.clone()),
                ("step".into(), main_.clone()),
                ("task.stopping".into(), main_.clone()),
                ("approval.requested".into(), main_.clone()),
                ("run.finished".into(), main_.clone()),
                ("memory.saved".into(), main_.clone()),
                ("memory.forgotten".into(), None),
                ("access.allowed".into(), None),
                ("settings".into(), None),
                ("secret.saved".into(), None),
                ("schedule.updated".into(), None),
                ("not.json".into(), None),
            ]
        );
        // The apps read it from the event itself.
        assert_eq!(store.events_after(1, 1).unwrap()[0].data["session"], "main");
        // An older agentd's statements still work on the newer database.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute_batch(
                "INSERT INTO history (role, content) VALUES ('user', 'from 0.15');
                 INSERT INTO runs (id, status, request, by_user, model, policy_version, schedule, acted)
                   VALUES ('old1', 'running', 'from 0.15', NULL, 'm', 'v', NULL, 0);
                 INSERT INTO events (kind, data) VALUES ('message', '{\"text\":\"from 0.15\"}');
                 INSERT INTO schedules (name, prompt, cron, timezone, next_run) VALUES ('Old', 'x', NULL, 'UTC', 1);",
            )
            .unwrap();
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(version, 9);
        }
        assert_eq!(
            store.history_in(MAIN, 10).unwrap().last().unwrap().text,
            "from 0.15"
        );
        assert!(store.schedules().unwrap().iter().all(|s| s.session == "main"));
        // An older agentd's chat task counts as main's.
        assert_eq!(store.latest_task().unwrap().unwrap().2, "main");
        drop(store);
        // Opening again doesn't migrate again.
        let again = Store::open(&path).unwrap();
        assert_eq!(again.sessions(false).unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrates_an_old_history_table() {
        let dir = std::env::temp_dir().join(format!("agentd-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agentd.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE history (id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL DEFAULT '',
                 role TEXT NOT NULL, content TEXT NOT NULL);
                 INSERT INTO history (role, content) VALUES ('user', 'hi');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        store.add_history("assistant", "hello").unwrap();
        assert_eq!(store.history(10).unwrap().len(), 2);
        drop(store);
        Store::open(&path).unwrap(); // a second open doesn't migrate again
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
