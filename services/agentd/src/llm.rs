//! Together AI chat completions (OpenAI-compatible API).

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::Value;

const API_URL: &str = "https://api.together.ai/v1/chat/completions";
const RETRYABLE: [u16; 5] = [429, 500, 502, 503, 504];

/// USD per 1M tokens: input, cached input, output (Together serverless, 2026-10-30).
pub fn prices(model: &str) -> Option<(f64, f64, f64)> {
    match model {
        "zai-org/GLM-5.3-Flash" => Some((0.15, 0.03, 0.50)),
        "zai-org/GLM-5.3" => Some((1.40, 0.26, 4.40)),
        "moonshotai/Kimi-K3" => Some((3.00, 0.30, 15.00)),
        _ => None,
    }
}

pub const KNOWN_MODELS: [&str; 3] = ["zai-org/GLM-5.3", "zai-org/GLM-5.3-Flash", "moonshotai/Kimi-K3"];

#[derive(Clone)]
pub struct Together {
    http: reqwest::Client,
    api_key: String,
}

impl Together {
    pub fn new(api_key: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Cloudflare in front of the API rejects some default User-Agents (error 1010).
            .user_agent("cloud-desktops-agentd/0.1")
            .timeout(Duration::from_secs(180))
            .build()?;
        Ok(Self { http, api_key })
    }

    /// POST a chat completion, retrying rate limits and outages.
    pub async fn chat(&self, body: &Value) -> Result<Value> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self
                .http
                .post(API_URL)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await;
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
                    retry_after.unwrap_or(1 << attempt)
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

#[derive(Clone, Debug, Default, Serialize)]
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
