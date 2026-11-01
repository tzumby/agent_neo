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
}

const SCHEDULE_COLUMNS: &str = "id, name, prompt, cron, timezone, next_run, last_run, paused, created";

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
}

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

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

    // --- Runs ---------------------------------------------------------------------

    pub fn start_run(&self, id: &str, request: &str, by: Option<&str>, model: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO runs (id, status, request, by_user, model) VALUES (?1, 'running', ?2, ?3, ?4)",
            params![id, request, by, model],
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

    // --- What the user is asked ---------------------------------------------------

    /// Parks a run on an approval: the card the user sees and the run's state.
    pub fn park(&self, id: &str, run: &str, card: &Value, checkpoint: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pending (id, kind, run, card, checkpoint) VALUES (?1, 'approval', ?2, ?3, ?4)",
            params![id, run, card.to_string(), checkpoint],
        )?;
        Ok(())
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
                    "SELECT id, run, card, checkpoint, created FROM pending WHERE {condition} ORDER BY created DESC LIMIT 1"
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
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        // Whose thread a history entry belongs to: "main" until there are bots.
        if conn.prepare("SELECT agent FROM history LIMIT 0").is_err() {
            conn.execute_batch("ALTER TABLE history ADD COLUMN agent TEXT NOT NULL DEFAULT 'main'")?;
        }
        conn.execute_batch("PRAGMA user_version = 1")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .start_run("r1", "do it", Some("me@example.com"), "m")
            .unwrap();
        assert_eq!(store.runs_with_status("running").unwrap(), ["r1"]);
        let card = serde_json::json!({"kind": "shell", "command": "sudo apt update"});
        store.park("a1", "r1", &card, "{\"step\":3}").unwrap();
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
