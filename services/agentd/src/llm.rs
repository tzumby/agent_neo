//! The model provider, with its key in agentd's secrets file (config.rs): one
//! per desktop. Together AI's chat completions (OpenAI-compatible: Together,
//! or another such endpoint at LLM_URL, such as Bedrock's for the evals), or
//! Anthropic's Messages API for Claude (claude.rs translates). The rest of
//! agentd speaks the OpenAI-style format either way.

use std::time::Duration;

use anyhow::{Result, anyhow};
use secrecy::{ExposeSecret, SecretString};

use crate::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOGETHER_URL: &str = "https://api.together.ai/v1/chat/completions";
/// 529: Anthropic's API is overloaded.
const RETRYABLE: [u16; 6] = [429, 500, 502, 503, 504, 529];
/// How long a Claude turn may take: replies aren't streamed, and one with
/// thinking can run for minutes.
const CLAUDE_TIMEOUT_SECS: u64 = 600;
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
        // price). Only for the evals (LLM_URL): not a desktop's model.
        "zai.glm-4.7-flash" => Some((0.07, 0.07, 0.40)),
        // Anthropic (2026-11-03). The whole prompt is cached (claude.rs), so
        // the input that isn't read from the cache is written to it: priced
        // as a cache write, 1.25 times the base input price. Haiku 5.5's is
        // for prompts up to 100K tokens.
        "claude-opus-4-6" => Some((6.25, 0.50, 25.00)),
        "claude-opus-5-5" => Some((5.00, 0.20, 20.00)),
        "claude-haiku-5-5" => Some((0.125, 0.01, 0.50)),
        _ => None,
    }
}

/// Who a desktop's models come from (LLM_PROVIDER).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Provider {
    Together,
    Anthropic,
}

impl Provider {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "together" => Some(Self::Together),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Together => "Together",
            Self::Anthropic => "Anthropic",
        }
    }

    /// The models the desktop's tasks may use, as the apps list them; the
    /// first is the default.
    pub fn models(self) -> &'static [&'static str] {
        match self {
            Self::Together => &KNOWN_MODELS,
            Self::Anthropic => &crate::claude::MODELS,
        }
    }

    pub fn allows(self, model: &str) -> bool {
        self.models().contains(&model)
    }

    /// Memory tidy-up's default, a smaller model.
    pub fn helper(self) -> &'static str {
        match self {
            Self::Together => crate::consolidate::DEFAULT_MODEL,
            Self::Anthropic => crate::claude::HELPER,
        }
    }

    /// The models memory tidy-up may use: the tasks' and the helper.
    pub fn allows_helper(self, model: &str) -> bool {
        self.allows(model) || model == self.helper()
    }

    pub fn default_url(self) -> &'static str {
        match self {
            Self::Together => TOGETHER_URL,
            Self::Anthropic => crate::claude::URL,
        }
    }
}

/// Together's models, as the app lists them. DeepSeek's are pinned to dated
/// snapshots, so a model doesn't change under a running desktop.
pub const KNOWN_MODELS: [&str; 5] = [
    "zai-org/GLM-5.3",
    "zai-org/GLM-5.3-Flash",
    "moonshotai/Kimi-K3",
    "deepseek-ai/DeepSeek-V4-Pro-0813",
    "deepseek-ai/DeepSeek-V4.1-Flash-1017",
];

/// Whether the model takes images (screenshots for app_gui; vision.rs).
/// Checked against Together's API with an image, 2026-11-25: Kimi-K3 reads
/// one; GLM-5.3 refuses image input (`multimodal_not_supported`), and the
/// others weren't tried. Claude's models all read images.
pub fn sees_images(model: &str) -> bool {
    model == "moonshotai/Kimi-K3" || model.starts_with("claude-")
}

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

pub struct Llm {
    http: reqwest::Client,
    api_key: SecretString,
    url: String,
    provider: Provider,
}

impl Llm {
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
            provider: cfg.provider,
        })
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response> {
        let request = self.http.post(&self.url);
        let request = match self.provider {
            Provider::Together => request.bearer_auth(self.api_key.expose_secret()),
            Provider::Anthropic => {
                let request = request
                    .header("x-api-key", self.api_key.expose_secret())
                    .header("anthropic-version", crate::claude::VERSION)
                    .timeout(Duration::from_secs(CLAUDE_TIMEOUT_SECS));
                let betas = crate::claude::betas(body["model"].as_str().unwrap_or_default());
                if betas.is_empty() {
                    request
                } else {
                    request.header("anthropic-beta", betas.join(","))
                }
            }
        };
        Ok(request.json(body).send().await?)
    }

    /// A chat completion (OpenAI-style, whoever the provider is), retrying
    /// rate limits and outages.
    pub async fn chat(&self, body: &Value) -> Result<Value> {
        let provider = self.provider.name();
        let body = match self.provider {
            Provider::Together => body.clone(),
            Provider::Anthropic => crate::claude::request(body),
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self.send(&body).await;
            let delay = match result {
                Ok(response) if response.status().is_success() => {
                    let reply: Value = response.json().await?;
                    return Ok(match self.provider {
                        Provider::Together => reply,
                        Provider::Anthropic => crate::claude::reply(&reply),
                    });
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                    let detail = response.text().await.unwrap_or_default();
                    if !RETRYABLE.contains(&status) || attempt >= 6 {
                        return Err(anyhow!(
                            "{provider} API error {status}: {}",
                            truncate(&detail, 500)
                        ));
                    }
                    retry_after.unwrap_or(1 << attempt).min(MAX_RETRY_DELAY_SECS)
                }
                Err(err) => {
                    if attempt >= 6 {
                        return Err(anyhow!("cannot reach {provider}: {err}"));
                    }
                    1 << attempt
                }
            };
            tracing::warn!(attempt, delay, provider, "model request failed, retrying");
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
