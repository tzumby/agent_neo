//! SQLite state: the event log the apps stream, the chat history the model
//! sees, and settings.

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
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);"
        ))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
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
}
