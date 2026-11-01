//! Push notifications through the push relay: a Lambda in the AWS account
//! that holds the APNs key. agentd signs its calls with this desktop's
//! instance-role credentials (SigV4), and the relay only reaches devices that
//! were registered through this desktop. The key never comes here.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::aws::FunctionUrl;
use crate::config::Config;

/// One notification (the relay checks and trims it again).
#[derive(Clone)]
pub struct Push {
    pub title: String,
    pub body: String,
    /// Notification category in the app (APPROVAL has Approve/Deny).
    pub category: Option<&'static str>,
    /// Groups a task's notifications together.
    pub thread: String,
    /// Short ids for the app: task, approval, action.
    pub data: Value,
}

pub struct Relay {
    relay: FunctionUrl,
}

impl Relay {
    /// None when no relay is configured (push is off).
    pub fn from_config(cfg: &Config) -> Result<Option<Self>> {
        if cfg.push_relay_url.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            relay: FunctionUrl::new(&cfg.push_relay_url, Duration::from_secs(20))?,
        }))
    }

    pub async fn register(&self, token: &str, name: &str) -> Result<()> {
        self.post("/devices", &json!({"token": token, "name": name}))
            .await
    }

    pub async fn notify(&self, push: &Push) -> Result<()> {
        let body = json!({
            "title": push.title, "body": push.body, "category": push.category,
            "thread": push.thread, "data": push.data,
        });
        self.post("/notify", &body).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<()> {
        let response = self.relay.post(path, &serde_json::to_vec(body)?).await?;
        let status = response.status();
        if !status.is_success() {
            bail!(
                "push relay answered {status}: {}",
                response.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }
}

impl Push {
    /// As stored in a `push` effect (effects.rs).
    pub fn payload(&self) -> Value {
        json!({
            "title": self.title, "body": self.body, "category": self.category,
            "thread": self.thread, "data": self.data,
        })
    }

    pub fn from_payload(payload: &Value) -> Option<Self> {
        let category = match payload["category"].as_str() {
            Some("APPROVAL") => Some("APPROVAL"),
            Some("ACTION") => Some("ACTION"),
            _ => None,
        };
        Some(Self {
            title: payload["title"].as_str()?.to_string(),
            body: payload["body"].as_str().unwrap_or_default().to_string(),
            category,
            thread: payload["thread"].as_str().unwrap_or_default().to_string(),
            data: payload["data"].clone(),
        })
    }

    /// The effect's key: `task:<task>:<category>`, plus the card it's for, so
    /// a task's distinct notifications never replace each other.
    pub fn effect_key(&self) -> String {
        let card = self.data["approval"]
            .as_str()
            .or_else(|| self.data["action"].as_str())
            .map(|id| format!(":{id}"))
            .unwrap_or_default();
        format!("task:{}:{}{card}", self.thread, self.category.unwrap_or("none"))
    }
}

/// What a scheduled run reported with `report_outcome` (docs/memory-v2.md §11).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reported {
    /// result, nothing or blocked.
    pub outcome: String,
    /// At most 180 characters: the notification's text.
    pub summary: String,
}

/// The attention budget (docs/memory-v2.md §11): the notification for a
/// finished task (title, body), or None when the phone should stay quiet.
/// The phone hears when the user must act or can use a result, and at no
/// other time.
pub fn on_finish(
    desktop: &str,
    schedule: Option<&str>,
    status: &str,
    reported: Option<&Reported>,
    answer: &str,
) -> Option<(String, String)> {
    let problem = matches!(status, "error" | "interrupted" | "stuck" | "max_steps");
    match (schedule, status, reported) {
        // Action needed already pushed; a stop came from the user.
        (_, "needs_user" | "cancelled", _) => None,
        (Some(name), _, _) if problem => Some((format!("{name}: a problem"), clip(answer, 180))),
        (None, _, _) if problem => Some(("Your desktop hit a problem".into(), clip(answer, 180))),
        (Some(_), _, Some(r)) if r.outcome == "nothing" => None,
        (Some(name), _, Some(r)) if r.outcome == "blocked" => {
            Some((format!("{name}: needs you"), clip(&r.summary, 180)))
        }
        (Some(name), _, Some(r)) => Some((name.to_string(), clip(&r.summary, 180))),
        // No report: fail open, as before.
        (Some(name), _, None) => Some((name.to_string(), clip(answer, 180))),
        (None, _, _) => Some((desktop.to_string(), clip(answer, 180))),
    }
}

/// Cuts text for a notification line.
pub fn clip(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let kept: String = flat.chars().take(max_chars - 1).collect();
    format!("{}…", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reported(outcome: &str) -> Reported {
        Reported {
            outcome: outcome.into(),
            summary: "3 unread from Dana about the contract".into(),
        }
    }

    #[test]
    fn the_phone_hears_only_what_needs_it() {
        let finish = |schedule, status, r: Option<&Reported>| {
            on_finish("cloud-desktop-dev", schedule, status, r, "The answer.")
        };
        let inbox = Some("Morning inbox");
        // A user's own task: the answer, or the problem.
        assert_eq!(
            finish(None, "done", None),
            Some(("cloud-desktop-dev".into(), "The answer.".into()))
        );
        for status in ["error", "interrupted", "stuck", "max_steps"] {
            assert_eq!(
                finish(None, status, None).unwrap().0,
                "Your desktop hit a problem"
            );
            assert_eq!(finish(inbox, status, None).unwrap().0, "Morning inbox: a problem");
        }
        // Already pushed (Action needed) or stopped by the user: nothing.
        for status in ["needs_user", "cancelled"] {
            assert_eq!(finish(None, status, None), None);
            assert_eq!(finish(inbox, status, Some(&reported("result"))), None);
        }
        // A scheduled run: its summary when there's a result or it's blocked…
        let result = reported("result");
        assert_eq!(
            finish(inbox, "done", Some(&result)),
            Some((
                "Morning inbox".into(),
                "3 unread from Dana about the contract".into()
            ))
        );
        assert_eq!(
            finish(inbox, "done", Some(&reported("blocked"))).unwrap().0,
            "Morning inbox: needs you"
        );
        // …nothing at all when there was nothing…
        assert_eq!(finish(inbox, "done", Some(&reported("nothing"))), None);
        // …and the answer when it didn't report (fail open).
        assert_eq!(
            finish(inbox, "done", None),
            Some(("Morning inbox".into(), "The answer.".into()))
        );
    }

    #[test]
    fn pushes_round_trip_through_an_effect_with_distinct_keys() {
        let card = Push {
            title: "Run this command?".into(),
            body: "rm -rf build".into(),
            category: Some("APPROVAL"),
            thread: "t1".into(),
            data: json!({"task": "t1", "approval": "a9"}),
        };
        let back = Push::from_payload(&card.payload()).unwrap();
        assert_eq!(
            (back.title.as_str(), back.category, back.data.clone()),
            ("Run this command?", Some("APPROVAL"), card.data.clone())
        );
        assert_eq!(card.effect_key(), "task:t1:APPROVAL:a9");
        let done = Push {
            category: None,
            data: json!({"task": "t1"}),
            ..card
        };
        assert_eq!(done.effect_key(), "task:t1:none");
        assert!(Push::from_payload(&json!({"body": "no title"})).is_none());
    }

    #[test]
    fn clips_long_text() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("one\ntwo   three", 50), "one two three");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
    }
}
