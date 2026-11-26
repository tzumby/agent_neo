//! Claude through Anthropic's Messages API, over raw HTTP (Rust has no
//! official SDK). The rest of agentd speaks the OpenAI-style chat format it
//! uses with Together, so a request is translated here on the way out and the
//! reply on the way back (llm.rs). A reply's content blocks travel with its
//! message in the task's history (`claude_content`) and go back verbatim, so
//! thinking blocks keep their signatures across a task's turns.

use serde_json::{Value, json};

pub const URL: &str = "https://api.anthropic.com/v1/messages";
/// The API version these requests are written for.
pub const VERSION: &str = "2026-10-21";
/// The chat models a desktop on Claude offers; the first is its default.
pub const MODELS: [&str; 2] = ["claude-opus-4-6", "claude-opus-5-5"];
/// Memory tidy-up's model: the current Haiku, as GLM-5.3-Flash is on Together.
pub const HELPER: &str = "claude-haiku-5-5";
/// Thinking counts against `max_tokens`, so a turn gets at least this much
/// room, and at most the second: requests aren't streamed (llm.rs allows
/// them ten minutes).
const MIN_MAX_TOKENS: u64 = 16_000;
const MAX_MAX_TOKENS: u64 = 32_000;
/// The answer when the model (and its fallback) declined and wrote nothing.
const DECLINED: &str = "The model declined to continue with this request.";
/// Stands in for content a message must have but doesn't.
const NOTHING: &str = "…";

/// Opus 5.5: refusal fallbacks (a policy decline is retried on the model the
/// API picks, in the same call), and replayed thinking that's dropped rather
/// than refused when agentd's own edits to the history (trimmed tool output,
/// older screenshots replaced by a line of text, the system prompt rebuilt
/// when a parked task resumes) no longer match the conversation it came from.
fn is_opus_5_5(model: &str) -> bool {
    model == "claude-opus-5-5"
}

/// The `anthropic-beta` header's features for a model's requests.
pub fn betas(model: &str) -> Vec<&'static str> {
    if is_opus_5_5(model) {
        vec![
            "server-side-fallback-2026-07-01",
            "thinking-binding-controls-2026-08-01",
        ]
    } else {
        Vec::new()
    }
}

/// An OpenAI-style chat request as a Messages API request.
pub fn request(body: &Value) -> Value {
    let model = body["model"].as_str().unwrap_or_default();
    let mut system: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for message in body["messages"].as_array().into_iter().flatten() {
        match message["role"].as_str() {
            // The system prompt leads; one later in the conversation (none
            // today) is the operator's word, kept in place as text.
            Some("system") if messages.is_empty() => {
                if let Some(text) = message["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    system.push(text.to_string());
                }
            }
            Some("system") => {
                let text = message["content"].as_str().unwrap_or_default();
                push(
                    &mut messages,
                    "user",
                    vec![text_block(&format!("(agentd) {text}"))],
                );
            }
            Some("user") => push(&mut messages, "user", user_blocks(&message["content"])),
            Some("assistant") => push(&mut messages, "assistant", assistant_blocks(message)),
            Some("tool") => push(&mut messages, "user", vec![tool_result(message)]),
            _ => {}
        }
    }
    answer_every_call(&mut messages);
    // A conversation starts with the user (agentd's notes from earlier are an
    // assistant turn) and ends with them: a last assistant turn would be a
    // prefill, which these models refuse.
    if messages.first().is_some_and(|m| m["role"] == "assistant") {
        messages.insert(
            0,
            json!({"role": "user", "content": [text_block("(agentd) Notes from earlier follow.")]}),
        );
    }
    if messages.last().is_none_or(|m| m["role"] == "assistant") {
        messages.push(json!({"role": "user", "content": [text_block("(agentd) Continue.")]}));
    }
    let asked = body["max_tokens"].as_u64().unwrap_or(MIN_MAX_TOKENS);
    let mut out = json!({
        "model": model,
        "max_tokens": asked.clamp(MIN_MAX_TOKENS, MAX_MAX_TOKENS),
        "messages": messages,
        "thinking": {"type": "adaptive"},
        // The whole prefix is cached: each turn reads what the last one wrote.
        "cache_control": {"type": "ephemeral"},
    });
    if !system.is_empty() {
        out["system"] = json!(system.join("\n\n"));
    }
    // Agentic work, at the effort Anthropic suggests for it (Opus 5.5's
    // default is a level lower).
    if model.starts_with("claude-opus") {
        out["output_config"] = json!({"effort": "high"});
    }
    let tools: Vec<Value> = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let function = &tool["function"];
            Some(json!({
                "name": function["name"].as_str()?,
                "description": function["description"].as_str().unwrap_or_default(),
                "input_schema": function["parameters"].clone(),
            }))
        })
        .collect();
    if !tools.is_empty() {
        out["tools"] = json!(tools);
        match body["tool_choice"].as_str() {
            Some("none") => out["tool_choice"] = json!({"type": "none"}),
            Some("auto") => out["tool_choice"] = json!({"type": "auto"}),
            _ => {}
        }
    }
    if is_opus_5_5(model) {
        out["fallbacks"] = json!("default");
        out["thinking"]["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
    }
    out
}

/// A Messages API reply as the OpenAI-style one agentd reads.
pub fn reply(response: &Value) -> Value {
    let content = response["content"].as_array().cloned().unwrap_or_default();
    // After a fallback, what the declined model wrote before the switch goes
    // back as text only: its thinking and calls aren't echoed (or run).
    let boundary = content.iter().rposition(|b| b["type"] == "fallback");
    let kept: Vec<Value> = content
        .into_iter()
        .enumerate()
        .filter(|(i, block)| {
            boundary.is_none_or(|b| *i >= b) || matches!(block["type"].as_str(), Some("text" | "fallback"))
        })
        .map(|(_, block)| block)
        .collect();
    let text = kept
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let calls: Vec<Value> = kept
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| {
            json!({"id": b["id"], "type": "function",
                   "function": {"name": b["name"], "arguments": b["input"].to_string()}})
        })
        .collect();
    let stop = response["stop_reason"].as_str().unwrap_or_default();
    let finish = match stop {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        _ => "stop",
    };
    let text = if text.trim().is_empty() && calls.is_empty() && stop == "refusal" {
        DECLINED.to_string()
    } else {
        text
    };
    let mut message = json!({
        "role": "assistant",
        "content": if text.trim().is_empty() { Value::Null } else { json!(text) },
        "claude_content": kept,
    });
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let usage = &response["usage"];
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let read = count("cache_read_input_tokens");
    json!({
        "model": response["model"],
        "choices": [{"message": message, "finish_reason": finish}],
        "usage": {
            "prompt_tokens": count("input_tokens") + read + count("cache_creation_input_tokens"),
            "completion_tokens": count("output_tokens"),
            "prompt_tokens_details": {"cached_tokens": read},
        },
    })
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// Appends a turn, or adds its blocks to the last one when it's the same
/// role (the API wants them alternating). A user turn's tool results stay
/// first in it.
fn push(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut().filter(|m| m["role"] == role)
        && let Some(content) = last["content"].as_array_mut()
    {
        content.extend(blocks);
        if role == "user" {
            content.sort_by_key(|b| b["type"] != "tool_result");
        }
        return;
    }
    messages.push(json!({"role": role, "content": blocks}));
}

fn user_blocks(content: &Value) -> Vec<Value> {
    let blocks: Vec<Value> = match content {
        Value::String(text) => vec![text_block(text)],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part["type"].as_str()? {
                "text" => Some(text_block(part["text"].as_str()?)),
                "image_url" => Some(image(part["image_url"]["url"].as_str()?)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let blocks: Vec<Value> = blocks
        .into_iter()
        .filter(|b| b["type"] != "text" || b["text"].as_str().is_some_and(|t| !t.trim().is_empty()))
        .collect();
    if blocks.is_empty() {
        vec![text_block(NOTHING)]
    } else {
        blocks
    }
}

/// A screenshot: a data URL as base64, anything else by its URL.
fn image(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((media_type, data)) = rest.split_once(";base64,")
    {
        return json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}});
    }
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

/// A reply of the model's own goes back as it came; one from elsewhere (a
/// test, an older task) is rebuilt from its text and calls.
fn assistant_blocks(message: &Value) -> Vec<Value> {
    if let Some(blocks) = message["claude_content"].as_array()
        && blocks
            .iter()
            .any(|b| matches!(b["type"].as_str(), Some("text" | "tool_use")))
    {
        return blocks.clone();
    }
    let mut blocks = Vec::new();
    if let Some(text) = message["content"].as_str().filter(|t| !t.trim().is_empty()) {
        blocks.push(text_block(text));
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        let input = call["function"]["arguments"]
            .as_str()
            .and_then(|args| serde_json::from_str::<Value>(args).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        blocks.push(json!({
            "type": "tool_use", "id": call_id(&call["id"]),
            "name": call["function"]["name"], "input": input,
        }));
    }
    // A reply with nothing to show (all thinking, cut off): a placeholder.
    if blocks.is_empty() {
        blocks.push(text_block(NOTHING));
    }
    blocks
}

fn tool_result(message: &Value) -> Value {
    let text = message["content"].as_str().unwrap_or_default();
    json!({
        "type": "tool_result", "tool_use_id": call_id(&message["tool_call_id"]),
        "content": if text.trim().is_empty() { "(no output)" } else { text },
    })
}

/// A call's id as the API takes it: letters, digits, `_` and `-`.
fn call_id(id: &Value) -> String {
    let id: String = id
        .as_str()
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if id.is_empty() { "call".into() } else { id }
}

/// Every call gets its result in the next turn, as the API requires: one a
/// task never ran (it was stopped with calls still queued) says so.
fn answer_every_call(messages: &mut Vec<Value>) {
    let mut i = 0;
    while i < messages.len() {
        if messages[i]["role"] != "assistant" {
            i += 1;
            continue;
        }
        let ids: Vec<String> = messages[i]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_use")
            .filter_map(|b| b["id"].as_str().map(String::from))
            .collect();
        if ids.is_empty() {
            i += 1;
            continue;
        }
        if messages.get(i + 1).is_none_or(|next| next["role"] != "user") {
            messages.insert(i + 1, json!({"role": "user", "content": []}));
        }
        let next = messages[i + 1]["content"]
            .as_array_mut()
            .expect("a user turn's content is a list");
        let answered: Vec<String> = next
            .iter()
            .filter(|b| b["type"] == "tool_result")
            .filter_map(|b| b["tool_use_id"].as_str().map(String::from))
            .collect();
        let missing: Vec<Value> = ids
            .iter()
            .filter(|id| !answered.contains(id))
            .map(|id| json!({"type": "tool_result", "tool_use_id": id, "content": "not run: the task stopped first"}))
            .collect();
        next.splice(0..0, missing);
        i += 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, args: &str) -> Value {
        json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}})
    }

    #[test]
    fn a_task_turn_becomes_a_messages_request() {
        let body = json!({
            "model": "claude-opus-4-6", "max_tokens": 8192, "tool_choice": "auto",
            "tools": [{"type": "function", "function": {
                "name": "read_text", "description": "Read the page",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false},
            }}],
            "messages": [
                {"role": "system", "content": "You are an assistant."},
                {"role": "assistant", "content": "Earlier notes and reports as quoted data: []"},
                {"role": "user", "content": "Read the page"},
                {"role": "assistant", "content": null, "tool_calls": [
                    call("call_1", "read_text", "{}"), call("call.2", "snapshot", "not json"),
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "<untrusted>Hello</untrusted>"},
                {"role": "tool", "tool_call_id": "call.2", "content": ""},
                {"role": "user", "content": [
                    {"type": "text", "text": "(agentd) The screenshots your tools took"},
                    {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,AAAA"}},
                    {"type": "text", "text": "[an earlier screenshot, no longer attached]"},
                ]},
            ],
        });
        let out = request(&body);
        assert_eq!(out["system"], "You are an assistant.");
        assert_eq!(out["max_tokens"], 16_000);
        assert_eq!(out["thinking"], json!({"type": "adaptive"}));
        assert_eq!(out["output_config"]["effort"], "high");
        assert_eq!(out["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(out["tool_choice"], json!({"type": "auto"}));
        assert_eq!(out["tools"][0]["input_schema"]["type"], "object");
        assert!(out.get("fallbacks").is_none() && out.get("temperature").is_none());
        let messages = out["messages"].as_array().unwrap();
        // Starts with the user, alternates, and ends with them.
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant", "user"]);
        let calls = &messages[3]["content"];
        assert_eq!(
            calls[0],
            json!({"type": "tool_use", "id": "call_1", "name": "read_text", "input": {}})
        );
        assert_eq!(
            (calls[1]["id"].as_str(), &calls[1]["input"]),
            (Some("call_2"), &json!({}))
        );
        // Both results first, in one turn, then the screenshot.
        let last = messages[4]["content"].as_array().unwrap();
        assert_eq!(last[0]["tool_use_id"], "call_1");
        assert_eq!(
            (last[1]["tool_use_id"].as_str(), last[1]["content"].as_str()),
            (Some("call_2"), Some("(no output)"))
        );
        assert_eq!(
            last[3],
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AAAA"}})
        );
        assert_eq!(last.len(), 5);
    }

    #[test]
    fn opus_5_5_gets_fallbacks_and_drops_thinking_its_history_no_longer_matches() {
        let body = json!({"model": "claude-opus-5-5", "max_tokens": 64_000,
                          "messages": [{"role": "user", "content": "hi"}]});
        let out = request(&body);
        assert_eq!(out["fallbacks"], "default");
        assert_eq!(
            out["thinking"]["block_binding"]["prefix_mismatch_behavior"],
            "drop_block"
        );
        assert_eq!(out["max_tokens"], 32_000);
        assert_eq!(betas("claude-opus-5-5").len(), 2);
        assert!(betas("claude-opus-4-6").is_empty());
        // The helper gets no Opus settings.
        let helper = request(&json!({"model": HELPER, "messages": [{"role": "user", "content": "hi"}]}));
        assert!(helper.get("output_config").is_none() && helper.get("fallbacks").is_none());
        assert!(helper.get("tools").is_none() && helper.get("system").is_none());
    }

    #[test]
    fn the_models_own_blocks_go_back_as_they_came() {
        let response = json!({
            "model": "claude-opus-4-6", "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "Let me look.", "signature": "sig=="},
                {"type": "text", "text": "Reading it now."},
                {"type": "tool_use", "id": "toolu_01", "name": "read_text", "input": {"max_chars": 500}},
            ],
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 1000, "cache_read_input_tokens": 9000, "output_tokens": 50},
        });
        let out = reply(&response);
        let message = &out["choices"][0]["message"];
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(message["content"], "Reading it now.");
        assert_eq!(message["tool_calls"][0]["id"], "toolu_01");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "{\"max_chars\":500}"
        );
        assert_eq!(out["usage"]["prompt_tokens"], 10_010);
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 9000);
        assert_eq!(out["usage"]["completion_tokens"], 50);
        // Next turn: the thinking block and its signature, unchanged.
        let next = request(&json!({"model": "claude-opus-4-6", "messages": [
            {"role": "user", "content": "Read the page"},
            message,
            {"role": "tool", "tool_call_id": "toolu_01", "content": "Hello"},
        ]}));
        assert_eq!(next["messages"][1]["content"], response["content"]);
        // A reply that was all thinking (cut off) goes back as a placeholder.
        let cut = reply(&json!({"stop_reason": "max_tokens", "content": [
            {"type": "thinking", "thinking": "Hmm", "signature": "s"}]}));
        assert_eq!(cut["choices"][0]["finish_reason"], "length");
        assert!(cut["choices"][0]["message"]["content"].is_null());
        let again = request(&json!({"model": "claude-opus-4-6", "messages": [
            {"role": "user", "content": "Go"}, cut["choices"][0]["message"],
            {"role": "user", "content": "(agentd) Your last reply was cut off"},
        ]}));
        assert_eq!(
            again["messages"][1]["content"],
            json!([{"type": "text", "text": NOTHING}])
        );
    }

    #[test]
    fn a_fallback_keeps_only_what_can_go_back_and_a_refusal_says_so() {
        let out = reply(&json!({
            "model": "claude-opus-5", "stop_reason": "end_turn",
            "content": [
                {"type": "thinking", "thinking": "", "signature": "declined"},
                {"type": "tool_use", "id": "toolu_x", "name": "click", "input": {}},
                {"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-5"}},
                {"type": "thinking", "thinking": "", "signature": "ok"},
                {"type": "text", "text": "Done."},
            ],
        }));
        let message = &out["choices"][0]["message"];
        let kinds: Vec<&str> = message["claude_content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["fallback", "thinking", "text"]);
        assert!(message.get("tool_calls").is_none());
        let refused = reply(&json!({"stop_reason": "refusal", "content": []}));
        assert_eq!(refused["choices"][0]["message"]["content"], DECLINED);
        assert_eq!(refused["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn a_stopped_tasks_queued_calls_are_answered_and_a_wrap_up_takes_no_tools() {
        let out = request(&json!({
            "model": "claude-opus-4-6", "tool_choice": "none",
            "tools": [{"type": "function", "function": {"name": "read_text", "parameters": {"type": "object"}}}],
            "messages": [
                {"role": "user", "content": "Read both"},
                {"role": "assistant", "content": "", "tool_calls": [call("a", "read_text", "{}"), call("b", "read_text", "{}")]},
                {"role": "tool", "tool_call_id": "a", "content": "first"},
                {"role": "user", "content": "(agentd) Stopped. Say what's done."},
            ],
        }));
        assert_eq!(out["tool_choice"], json!({"type": "none"}));
        let last = out["messages"][2]["content"].as_array().unwrap();
        let results: Vec<&str> = last.iter().filter_map(|b| b["tool_use_id"].as_str()).collect();
        assert_eq!(results, ["b", "a"]);
        assert_eq!(last[0]["content"], "not run: the task stopped first");
        assert_eq!(last[2]["type"], "text");
        // A call left without any turn after it gets one.
        let alone = request(&json!({"model": "claude-opus-4-6", "messages": [
            {"role": "user", "content": "Go"},
            {"role": "assistant", "content": null, "tool_calls": [call("c", "read_text", "{}")]},
        ]}));
        let roles: Vec<&str> = alone["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert_eq!(alone["messages"][2]["content"][0]["tool_use_id"], "c");
    }
}
