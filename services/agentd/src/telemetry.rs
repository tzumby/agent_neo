//! What each model call and tool call cost, to work out where a task's
//! tokens go (`make agentd-telemetry`). One `model_calls` row per call to
//! the model and one `tool_calls` row per tool call (store.rs), kept 90 days.
//!
//! Numbers and names only: token counts, sizes in characters, timings, tool
//! names, the plan step, and the prompt cache's verdict. Never the prompt,
//! page text, command output, a call's arguments or anything typed.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::Value;

/// What a request is made of, in characters (a rough guide to its tokens:
/// about 3.5 characters each for English and page outlines), and images.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Composition {
    pub messages: usize,
    pub system_chars: usize,
    /// The tool definitions, as JSON.
    pub tools_chars: usize,
    pub tool_result_chars: usize,
    /// The model's own turns: text, thinking and its calls' arguments.
    pub assistant_chars: usize,
    /// The user's messages and agentd's notes to the model.
    pub user_chars: usize,
    pub images: usize,
}

/// Measures an OpenAI-style request (llm.rs's format, before claude.rs).
pub fn composition(request: &Value) -> Composition {
    let mut parts = Composition {
        tools_chars: request["tools"]
            .as_array()
            .map_or(0, |tools| Value::Array(tools.clone()).to_string().len()),
        ..Composition::default()
    };
    for (i, message) in request["messages"].as_array().into_iter().flatten().enumerate() {
        parts.messages += 1;
        match message["role"].as_str() {
            Some("system") if i == 0 => parts.system_chars += text_len(&message["content"]),
            Some("tool") => parts.tool_result_chars += text_len(&message["content"]),
            Some("assistant") => parts.assistant_chars += assistant_len(message),
            _ => {
                parts.user_chars += text_len(&message["content"]);
                parts.images += images(&message["content"]);
            }
        }
    }
    parts
}

/// A message's text: a string, or its text parts.
fn text_len(content: &Value) -> usize {
    match content {
        Value::String(text) => text.len(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .map(str::len)
            .sum(),
        _ => 0,
    }
}

fn images(content: &Value) -> usize {
    content.as_array().map_or(0, |parts| {
        parts
            .iter()
            .filter(|p| matches!(p["type"].as_str(), Some("image_url" | "image_ref")))
            .count()
    })
}

/// An assistant turn: Claude's content blocks (text, thinking, calls) when
/// it has them, else its text, reasoning and calls' arguments.
fn assistant_len(message: &Value) -> usize {
    if let Some(blocks) = message["claude_content"].as_array() {
        return blocks
            .iter()
            .map(|block| match block["type"].as_str() {
                Some("text") => text_len(&block["text"]),
                Some("thinking") => text_len(&block["thinking"]),
                Some("tool_use") => block["input"].to_string().len(),
                _ => 0,
            })
            .sum();
    }
    let calls: usize = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|call| call["function"]["arguments"].as_str())
        .map(str::len)
        .sum();
    text_len(&message["content"]) + text_len(&message["reasoning_content"]) + calls
}

/// The task's previous request, for the next one: a hash of each message,
/// and Claude's response id (the API's cache diagnostics compare against
/// it). Not kept across a restart: the first request after one has none.
#[derive(Clone, Debug, Default)]
pub struct Sent {
    hashes: Vec<u64>,
    pub response: Option<String>,
}

impl Sent {
    /// How many of `messages` lead the same as the previous request's, and
    /// how many it had (None for a task's first request). Fewer kept than it
    /// had means agentd changed an earlier message (a cut, a rebuilt system
    /// prompt), so the prompt cache writes everything after it again.
    pub fn track(&mut self, messages: &[Value]) -> Option<(usize, usize)> {
        let hashes: Vec<u64> = messages.iter().map(hash).collect();
        let kept = (!self.hashes.is_empty()).then(|| {
            let same = self
                .hashes
                .iter()
                .zip(&hashes)
                .take_while(|(a, b)| a == b)
                .count();
            (same, self.hashes.len())
        });
        self.hashes = hashes;
        kept
    }
}

fn hash(message: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    message.to_string().hash(&mut hasher);
    hasher.finish()
}

/// A tool result's outcome, from agentd's own words at its start.
pub fn outcome(result: &str) -> String {
    if let Some(code) = result
        .strip_prefix("exit code ")
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit() && c != '-').next())
        .filter(|code| !code.is_empty())
    {
        return format!("exit {code}");
    }
    for (prefix, outcome) in [
        ("error:", "error"),
        ("blocked:", "blocked"),
        ("denied:", "denied"),
        ("not run:", "not_run"),
        ("not clicked", "not_done"),
        ("not typed", "not_done"),
        ("not done", "not_done"),
        ("not asked", "not_done"),
    ] {
        if result.starts_with(prefix) {
            return outcome.into();
        }
    }
    "ok".into()
}

/// The prompt cache's verdict on a Claude request (its `diagnostics`): why
/// it missed, and about how many input tokens came after the change. None
/// when it hit, or nothing was compared.
pub fn cache_miss(diagnostics: &Value) -> Option<(String, Option<i64>)> {
    let reason = &diagnostics["cache_miss_reason"];
    if diagnostics.is_object() && reason.is_null() && diagnostics.get("cache_miss_reason").is_some() {
        // Still being compared when the response was written.
        return Some(("inconclusive".into(), None));
    }
    let kind = reason["type"].as_str()?;
    Some((kind.to_string(), reason["cache_missed_input_tokens"].as_i64()))
}

/// One model call, for its `model_calls` row.
pub struct ModelCall<'a> {
    pub run: &'a str,
    /// `turn` (the task's own), `review` (the supervisor) or `wrap_up`.
    pub kind: &'a str,
    pub step: u32,
    /// The plan step under way (1-based), if the task has a plan.
    pub plan_step: Option<usize>,
    pub model: &'a str,
    pub ms: u64,
    /// The reply's `usage` (llm.rs's format).
    pub usage: &'a Value,
    pub cost: Option<f64>,
    pub finish: &'a str,
    /// The tools the reply called, in order.
    pub tools: Vec<String>,
    pub composition: Composition,
    pub kept: Option<(usize, usize)>,
    /// Old tool results cut just before it (guard::enforce_budget).
    pub cut: usize,
    pub cache_miss: Option<(String, Option<i64>)>,
}

impl ModelCall<'_> {
    pub fn tokens(&self, key: &str) -> u64 {
        match key {
            "cache_read" => self.usage["prompt_tokens_details"]["cached_tokens"].as_u64(),
            "cache_write" => self.usage["prompt_tokens_details"]["cache_write_tokens"].as_u64(),
            other => self.usage[other].as_u64(),
        }
        .unwrap_or(0)
    }
}

/// The plan step under way: the first in progress, else the first not done.
pub fn plan_step(plan: &[crate::plan::Item]) -> Option<usize> {
    use crate::plan::Status;
    plan.iter()
        .position(|item| item.status == Status::InProgress)
        .or_else(|| plan.iter().position(|item| item.status != Status::Done))
        .map(|i| i + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_is_measured_by_part() {
        let request = json!({
            "tools": [{"type": "function", "function": {"name": "click"}}],
            "messages": [
                {"role": "system", "content": "rules"},
                {"role": "user", "content": "do it"},
                {"role": "assistant", "content": null, "claude_content": [
                    {"type": "thinking", "thinking": "hmm"},
                    {"type": "tool_use", "input": {"ref": 1}},
                ]},
                {"role": "tool", "content": "clicked"},
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_ref", "id": "ab"},
                ]},
            ],
        });
        let parts = composition(&request);
        assert_eq!(parts.messages, 5);
        assert_eq!(parts.system_chars, 5);
        assert_eq!(parts.user_chars, "do it".len() + "look".len());
        assert_eq!(parts.assistant_chars, "hmm".len() + r#"{"ref":1}"#.len());
        assert_eq!(parts.tool_result_chars, "clicked".len());
        assert_eq!(parts.images, 1);
        assert!(parts.tools_chars > 20);
    }

    #[test]
    fn an_edit_to_an_earlier_message_shows_in_what_was_kept() {
        let mut sent = Sent::default();
        let a = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "u"}),
        ];
        assert_eq!(sent.track(&a), None);
        let mut b = a.clone();
        b.push(json!({"role": "tool", "content": "page"}));
        assert_eq!(sent.track(&b), Some((2, 2)));
        // The old result cut: the conversation changed from message 2 on.
        let mut c = b.clone();
        c[2] = json!({"role": "tool", "content": "[older tool output removed]"});
        c.push(json!({"role": "tool", "content": "next"}));
        assert_eq!(sent.track(&c), Some((2, 3)));
    }

    #[test]
    fn outcomes_come_from_agentds_own_words() {
        assert_eq!(outcome("exit code 0\nok"), "exit 0");
        assert_eq!(outcome("exit code 127\nbash: x: not found"), "exit 127");
        assert_eq!(outcome("denied: the user did not approve this"), "denied");
        assert_eq!(outcome("not typed: 'To' is disabled"), "not_done");
        assert_eq!(outcome("clicked 'Save' (ready after 20 ms)"), "ok");
    }

    #[test]
    fn the_caches_verdict_is_read_from_its_diagnostics() {
        let missed =
            json!({"cache_miss_reason": {"type": "messages_changed", "cache_missed_input_tokens": 41850}});
        assert_eq!(
            cache_miss(&missed),
            Some(("messages_changed".into(), Some(41850)))
        );
        let unknown = json!({"cache_miss_reason": {"type": "previous_message_not_found"}});
        assert_eq!(
            cache_miss(&unknown),
            Some(("previous_message_not_found".into(), None))
        );
        assert_eq!(
            cache_miss(&json!({"cache_miss_reason": null})),
            Some(("inconclusive".into(), None))
        );
        assert_eq!(cache_miss(&Value::Null), None);
    }
}
