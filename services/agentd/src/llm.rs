//! Together AI chat completions (OpenAI-compatible API), with the key in
//! agentd's secrets file (config.rs): Together, or another OpenAI-compatible
//! endpoint at LLM_URL, such as Bedrock's for the evals.

use std::time::Duration;

use anyhow::{Result, anyhow};
use secrecy::{ExposeSecret, SecretString};

use crate::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOGETHER_URL: &str = "https://api.together.ai/v1/chat/completions";
const RETRYABLE: [u16; 5] = [429, 500, 502, 503, 504];
/// Never wait longer than this between retries, whatever Retry-After says.
const MAX_RETRY_DELAY_SECS: u64 = 30;

/// USD per 1M tokens: input, cached input, output (Together serverless;
/// GLM and Kimi 2026-10-30, DeepSeek 2026-11-02), for the cost of each task.
pub fn prices(model: &str) -> Option<(f64, f64, f64)> {
    match model {
        "zai-org/GLM-5.3-Flash" => Some((0.15, 0.03, 0.50)),
        "zai-org/GLM-5.3" => Some((1.40, 0.26, 4.40)),
        "moonshotai/Kimi-K3" => Some((3.00, 0.30, 15.00)),
        "deepseek-ai/DeepSeek-V4-Pro-0813" => Some((1.32, 0.13, 3.96)),
        "deepseek-ai/DeepSeek-V4.1-Flash-1017" => Some((0.30, 0.006, 1.20)),
        // Bedrock us-east-1 (AWS Price List, 2026-11-02; no cached-input
        // price). Only for the evals (LLM_URL): not in KNOWN_MODELS.
        "zai.glm-4.7-flash" => Some((0.07, 0.07, 0.40)),
        _ => None,
    }
}

/// The models a desktop may use, as the app lists them. DeepSeek's are pinned
/// to dated snapshots, so a model doesn't change under a running desktop.
pub const KNOWN_MODELS: [&str; 5] = [
    "zai-org/GLM-5.3",
    "zai-org/GLM-5.3-Flash",
    "moonshotai/Kimi-K3",
    "deepseek-ai/DeepSeek-V4-Pro-0813",
    "deepseek-ai/DeepSeek-V4.1-Flash-1017",
];

/// Whether the model wants its `reasoning_content` sent back on later turns.
/// GLM and Kimi do (they reason worse without it); others, such as DeepSeek,
/// reject it in requests, so it's dropped for anything not known to want it.
pub fn keeps_reasoning(model: &str) -> bool {
    model.starts_with("zai-org/GLM") || model.starts_with("moonshotai/Kimi")
}

/// An assistant message as it goes back into the conversation.
pub fn for_history(message: &Value, model: &str) -> Value {
    let mut message = message.clone();
    if let Some(map) = message.as_object_mut() {
        map.retain(|_, v| !v.is_null());
        if !keeps_reasoning(model) {
            map.remove("reasoning_content");
            map.remove("reasoning");
        }
    }
    message
}

pub struct Together {
    http: reqwest::Client,
    api_key: SecretString,
    url: String,
}

impl Together {
    pub fn new(cfg: &Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Cloudflare in front of the API rejects some default User-Agents (error 1010).
            .user_agent("neo-agent-agentd/0.1")
            .timeout(Duration::from_secs(180))
            .build()?;
        Ok(Self {
            http,
            api_key: cfg.api_key.clone(),
            url: cfg.llm_url.clone(),
        })
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response> {
        Ok(self
            .http
            .post(&self.url)
            .bearer_auth(self.api_key.expose_secret())
            .json(body)
            .send()
            .await?)
    }

    /// POST a chat completion, retrying rate limits and outages.
    pub async fn chat(&self, body: &Value) -> Result<Value> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self.send(body).await;
            let delay = match result {
                Ok(response) if response.status().is_success() => return Ok(response.json().await?),
                Ok(response) => {
                    let status = response.status().as_u16();
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                    let detail = response.text().await.unwrap_or_default();
                    if !RETRYABLE.contains(&status) || attempt >= 6 {
                        return Err(anyhow!("Together API error {status}: {}", truncate(&detail, 500)));
                    }
                    retry_after.unwrap_or(1 << attempt).min(MAX_RETRY_DELAY_SECS)
                }
                Err(err) => {
                    if attempt >= 6 {
                        return Err(anyhow!("cannot reach Together: {err}"));
                    }
                    1 << attempt
                }
            };
            tracing::warn!(attempt, delay, "Together request failed, retrying");
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
}

impl Usage {
    pub fn add(&mut self, usage: Option<&Value>) {
        let Some(u) = usage else { return };
        self.input += u["prompt_tokens"].as_u64().unwrap_or(0);
        self.cached += u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
        self.output += u["completion_tokens"].as_u64().unwrap_or(0);
    }

    pub fn cost(&self, model: &str) -> Option<f64> {
        let (price_in, price_cached, price_out) = prices(model)?;
        let uncached = self.input.saturating_sub(self.cached) as f64;
        Some((uncached * price_in + self.cached as f64 * price_cached + self.output as f64 * price_out) / 1e6)
    }
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}\n[truncated: {} more characters]", count - max_chars)
}
