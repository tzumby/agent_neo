//! SQLite state: the event log the apps stream, the chat history the model
//! sees, runs (tasks) and what they wait on, the agent's memory, schedules,
//! the saved keys' names and labels (their values are files: secrets.rs), and
//! settings.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub data: Value,
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
}

const SCHEDULE_COLUMNS: &str =
    "id, name, prompt, cron, timezone, next_run, last_run, paused, created, policy_version, created_from";

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
                 last_used TEXT);"
        ))?;
        migrate(&conn)?;
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

    /// A task starts, under the rules in `policy_version` (policy.rs);
    /// `schedule` is set for a scheduled run.
    pub fn start_run(
        &self,
        id: &str,
        request: &str,
        by: Option<&str>,
        model: &str,
        policy_version: &str,
        schedule: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO runs (id, status, request, by_user, model, policy_version, schedule)
             VALUES (?1, 'running', ?2, ?3, ?4, ?5, ?6)",
            params![id, request, by, model, policy_version, schedule],
        )?;
        Ok(())
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
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!(
                "UPDATE runs SET status = ?2, steps = ?3, cost = ?4, answer = ?5, updated = {NOW} WHERE id = ?1"
            ),
            params![id, status, steps, cost, answer],
        )?;
        Ok(())
    }

    /// IDs of the runs with this status, oldest first.
    pub fn runs_with_status(&self, status: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM runs WHERE status = ?1 ORDER BY started")?;
        let rows = stmt.query_map(params![status], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- Memory -------------------------------------------------------------------

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
               (SELECT id, text, trusted, created FROM memory ORDER BY id DESC LIMIT ?1)
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![limit], memory_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn forget(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM memory WHERE id = ?1", params![id])? > 0)
    }

    /// The user vouches for an unverified memory.
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

    pub fn delete_schedule(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM schedules WHERE id = ?1", params![id])? > 0)
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

    /// Every parked approval: (id, card kind), newest first.
    pub fn parked_cards(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, COALESCE(json_extract(card, '$.kind'), 'shell') FROM pending
             WHERE kind = 'approval' ORDER BY created DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The latest task (not a consolidation): (id, status).
    pub fn latest_task(&self) -> Result<Option<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, status FROM runs WHERE kind = 'task' ORDER BY rowid DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Memory counts: (active, pending).
    pub fn memory_counts(&self) -> Result<(i64, i64)> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FILTER (WHERE status = 'active'), COUNT(*) FILTER (WHERE status = 'pending')
             FROM memory WHERE agent = 'main'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
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

    pub fn has_action(&self, id: &str) -> Result<bool> {
        Ok(self
            .pending_where("kind = 'action' AND id = ?1", Some(id))?
            .is_some())
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
                |row| {
                    let card: String = row.get(2)?;
                    Ok(Pending {
                        id: row.get(0)?,
                        run: row.get(1)?,
                        card: serde_json::from_str(&card).unwrap_or(Value::Null),
                        checkpoint: row.get(3)?,
                        created: row.get(4)?,
                        policy_version: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn append_event(&self, kind: &str, data: &Value) -> Result<Event> {
        let conn = self.conn.lock().unwrap();
        let (seq, ts) = conn.query_row(
            "INSERT INTO events (kind, data) VALUES (?1, ?2) RETURNING seq, ts",
            params![kind, data.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Event {
            seq,
            ts,
            kind: kind.to_string(),
            data: data.clone(),
        })
    }

    pub fn events_after(&self, after: i64, limit: i64) -> Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT seq, ts, kind, data FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let rows = stmt.query_map(params![after, limit], |row| {
            let data: String = row.get(3)?;
            Ok(Event {
                seq: row.get(0)?,
                ts: row.get(1)?,
                kind: row.get(2)?,
                data: serde_json::from_str(&data).unwrap_or(Value::Null),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn last_seq(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| row.get(0))?)
    }

    pub fn add_history(&self, role: &str, text: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO history (role, content) VALUES (?1, ?2)",
            params![role, text],
        )?;
        Ok(())
    }

    /// The latest `limit` entries, oldest first.
    pub fn history(&self, limit: i64) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, role, content FROM (SELECT id, ts, role, content FROM history ORDER BY id DESC LIMIT ?1)
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
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

/// Schema changes to databases made by earlier versions (PRAGMA user_version).
/// Every change is additive, so an older agentd still reads and writes a
/// newer database (it ignores versions above its own).
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        // Whose thread a history entry belongs to: "main" until there are bots.
        if conn.prepare("SELECT agent FROM history LIMIT 0").is_err() {
            conn.execute_batch("ALTER TABLE history ADD COLUMN agent TEXT NOT NULL DEFAULT 'main'")?;
        }
        conn.execute_batch("PRAGMA user_version = 1")?;
    }
    if version < 2 {
        // One transaction: a failed step leaves the database at version 1.
        conn.execute_batch(&format!("BEGIN; {MIGRATION_2} COMMIT;"))?;
    }
    Ok(())
}

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
    fn migrates_a_v1_database_to_v2() {
        let (store, dir) = migrated_v1();
        {
            let conn = store.conn.lock().unwrap();
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(version, 2);
            // Every existing memory is an active fact; trust follows `trusted`.
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
                (true, "user_stated", "active", "fact"),
                (false, "agent_inferred", "active", "fact"),
                (true, "user_stated", "active", "fact"),
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
            .effect_again(&first, 150, Some("gateway answered 503"))
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
