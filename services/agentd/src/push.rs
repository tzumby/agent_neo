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

    #[test]
    fn clips_long_text() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("one\ntwo   three", 50), "one two three");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
    }
}
