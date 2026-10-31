//! SQLite state: the event log the apps stream, the chat history the model
//! sees, runs (tasks) and what they wait on, and settings.

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
                 created INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)));"
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

    /// Any parked approval (there is at most one: one task at a time).
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
