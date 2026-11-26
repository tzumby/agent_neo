//! The model providers: Anthropic's Messages API for Claude (claude.rs
//! translates), and Together AI's chat completions (OpenAI-compatible:
//! Together, or another such endpoint at LLM_URL, such as Bedrock's for the
//! evals). The rest of agentd speaks the OpenAI-style format either way.
//!
//! A desktop may have a key for each provider: one the user pasted in the app
//! (`ModelKeys`), else the deployment's (secrets.env). A request goes to the
//! provider of its model, so switching is picking another model, with no
//! deploy or restart.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
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

/// Who a model comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Anthropic,
    Together,
}

impl Provider {
    /// Every provider, in the order the apps list them.
    pub const ALL: [Self; 2] = [Self::Anthropic, Self::Together];

    /// By its id (LLM_PROVIDER, the API's paths).
    pub fn parse(id: &str) -> Option<Self> {
        match id {
            "together" => Some(Self::Together),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Together => "together",
            Self::Anthropic => "anthropic",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Together => "Together",
            Self::Anthropic => "Anthropic",
        }
    }

    /// The provider whose model (or memory helper) this is; None for a model
    /// no provider lists, such as the evals' Bedrock one.
    pub fn of(model: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.allows_helper(model))
    }

    /// The models the provider's tasks may use, as the apps list them; the
    /// first is its default.
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

    /// The deployment's variable for its key (secrets.env).
    pub fn key_name(self) -> &'static str {
        match self {
            Self::Together => "TOGETHER_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
        }
    }
}

/// Every provider's task models, in the apps' order.
pub fn all_models() -> Vec<&'static str> {
    Provider::ALL
        .iter()
        .flat_map(|p| p.models().iter().copied())
        .collect()
}

/// Where a provider's key came from.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyFrom {
    /// Pasted in the app: kept in agentd's state directory.
    App,
    /// The deployment's, in /etc/agentd/secrets.env (Ansible, from SSM).
    Deployment,
}

/// The providers' keys. One pasted in the app is a file in agentd's state
/// directory (`model-keys/<provider>`, mode 0600 in a 0700 directory: agentd
/// and root only), apart from the keys commands use (secrets.rs): no command
/// ever gets it. It wins over the deployment's, which takes over again when
/// it's removed. Values are never logged or shown again.
pub struct ModelKeys {
    dir: PathBuf,
    deployment: HashMap<Provider, SecretString>,
    saved: RwLock<HashMap<Provider, SecretString>>,
}

impl ModelKeys {
    /// The keys saved in `dir` (created, mode 0700), over the deployment's.
    pub fn open(dir: &Path, deployment: &[(Provider, SecretString)]) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let mut saved = HashMap::new();
        for provider in Provider::ALL {
            match std::fs::read_to_string(dir.join(provider.id())) {
                Ok(value) if !value.trim().is_empty() => {
                    saved.insert(provider, SecretString::from(value.trim().to_string()));
                }
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => tracing::warn!(%err, provider = provider.id(), "couldn't read a model key"),
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            deployment: deployment.iter().cloned().collect(),
            saved: RwLock::new(saved),
        })
    }

    pub fn get(&self, provider: Provider) -> Option<SecretString> {
        self.saved
            .read()
            .unwrap()
            .get(&provider)
            .or_else(|| self.deployment.get(&provider))
            .cloned()
    }

    pub fn from(&self, provider: Provider) -> Option<KeyFrom> {
        if self.saved.read().unwrap().contains_key(&provider) {
            Some(KeyFrom::App)
        } else if self.deployment.contains_key(&provider) {
            Some(KeyFrom::Deployment)
        } else {
            None
        }
    }

    pub fn has(&self, provider: Provider) -> bool {
        self.from(provider).is_some()
    }

    /// Saves the app's key (replacing it): a temporary file with mode 0600,
    /// then renamed into place.
    pub fn put(&self, provider: Provider, value: &str) -> Result<()> {
        let value = crate::secrets::check_value(value).map_err(anyhow::Error::msg)?;
        let temporary = self.dir.join(format!(".{}.new", provider.id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(value.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, self.dir.join(provider.id()))?;
        self.saved
            .write()
            .unwrap()
            .insert(provider, SecretString::from(value.to_string()));
        Ok(())
    }

    /// Removes the app's key; false when there was none (a deployment's
    /// key stays: it's root's file).
    pub fn remove(&self, provider: Provider) -> Result<bool> {
        if self.saved.write().unwrap().remove(&provider).is_none() {
            return Ok(false);
        }
        match std::fs::remove_file(self.dir.join(provider.id())) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(err) => Err(err.into()),
        }
    }
}

/// What the provider said of a key before it was saved.
#[derive(Debug, PartialEq)]
pub enum KeyCheck {
    Accepted,
    /// It answered no (401, 403): the status.
    Refused(u16),
    /// It couldn't be asked (unreachable, an outage, an endpoint with no
    /// model list): why.
    Unchecked(String),
}

/// The provider's list of models, beside its chat endpoint: a free request
/// that needs a good key.
fn models_url(url: &str) -> Option<String> {
    ["/chat/completions", "/messages"]
        .iter()
        .find_map(|tail| url.strip_suffix(tail))
        .map(|base| format!("{base}/models"))
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
    pub keys: ModelKeys,
    /// The deployment's provider (LLM_PROVIDER): a model no provider lists
    /// goes to it.
    default: Provider,
    /// Its endpoint (LLM_URL); the others' are their usual ones.
    url: String,
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
            keys: ModelKeys::open(&cfg.state_dir.join("model-keys"), &cfg.keys)
                .context("opening the model keys")?,
            default: cfg.provider,
            url: cfg.llm_url.clone(),
        })
    }

    /// Who answers a request for this model.
    pub fn provider_for(&self, model: &str) -> Provider {
        Provider::of(model).unwrap_or(self.default)
    }

    fn url(&self, provider: Provider) -> &str {
        if provider == self.default {
            &self.url
        } else {
            provider.default_url()
        }
    }

    fn authorized(
        &self,
        request: reqwest::RequestBuilder,
        provider: Provider,
        key: &SecretString,
    ) -> reqwest::RequestBuilder {
        match provider {
            Provider::Together => request.bearer_auth(key.expose_secret()),
            Provider::Anthropic => request
                .header("x-api-key", key.expose_secret())
                .header("anthropic-version", crate::claude::VERSION),
        }
    }

    async fn send(&self, provider: Provider, key: &SecretString, body: &Value) -> Result<reqwest::Response> {
        let request = self.authorized(self.http.post(self.url(provider)), provider, key);
        let request = match provider {
            Provider::Together => request,
            Provider::Anthropic => {
                let request = request.timeout(Duration::from_secs(CLAUDE_TIMEOUT_SECS));
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

    /// Asks the provider whether it takes this key, before it's saved.
    pub async fn check_key(&self, provider: Provider, value: &str) -> KeyCheck {
        let Some(url) = models_url(self.url(provider)) else {
            return KeyCheck::Unchecked("this endpoint has no list of models to ask".into());
        };
        let key = SecretString::from(value.trim().to_string());
        let request = self
            .authorized(self.http.get(url), provider, &key)
            .timeout(Duration::from_secs(20));
        match request.send().await {
            Ok(response) if response.status().is_success() => KeyCheck::Accepted,
            Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
                KeyCheck::Refused(response.status().as_u16())
            }
            Ok(response) => {
                KeyCheck::Unchecked(format!("{} answered {}", provider.name(), response.status()))
            }
            Err(err) => KeyCheck::Unchecked(format!("couldn't reach {}: {err}", provider.name())),
        }
    }

    /// A chat completion (OpenAI-style, whoever the provider is), retrying
    /// rate limits and outages.
    pub async fn chat(&self, body: &Value) -> Result<Value> {
        let provider = self.provider_for(body["model"].as_str().unwrap_or_default());
        let Some(key) = self.keys.get(provider) else {
            return Err(anyhow!(
                "there's no {} API key on this desktop yet: add one in the app's Settings, under Model, \
                 and send the message again",
                provider.name()
            ));
        };
        let body = match provider {
            Provider::Together => body.clone(),
            Provider::Anthropic => crate::claude::request(body),
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self.send(provider, &key, &body).await;
            let delay = match result {
                Ok(response) if response.status().is_success() => {
                    let reply: Value = response.json().await?;
                    return Ok(match provider {
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
                    if status == 401 {
                        return Err(anyhow!(
                            "{} didn't accept this desktop's API key (401): replace it in the app's \
                             Settings, under Model",
                            provider.name()
                        ));
                    }
                    if !RETRYABLE.contains(&status) || attempt >= 6 {
                        return Err(anyhow!(
                            "{} API error {status}: {}",
                            provider.name(),
                            truncate(&detail, 500)
                        ));
                    }
                    retry_after.unwrap_or(1 << attempt).min(MAX_RETRY_DELAY_SECS)
                }
                Err(err) => {
                    if attempt >= 6 {
                        return Err(anyhow!("cannot reach {}: {err}", provider.name()));
                    }
                    1 << attempt
                }
            };
            tracing::warn!(
                attempt,
                delay,
                provider = provider.id(),
                "model request failed, retrying"
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_goes_to_the_provider_that_lists_it() {
        assert_eq!(Provider::of("claude-opus-4-6"), Some(Provider::Anthropic));
        assert_eq!(Provider::of("claude-haiku-5-5"), Some(Provider::Anthropic));
        assert_eq!(Provider::of("moonshotai/Kimi-K3"), Some(Provider::Together));
        assert_eq!(Provider::of("zai-org/GLM-5.3-Flash"), Some(Provider::Together));
        // The evals' Bedrock model: the deployment's provider takes it.
        assert_eq!(Provider::of("zai.glm-4.7-flash"), None);
        assert_eq!(all_models()[..2], crate::claude::MODELS);
        for provider in Provider::ALL {
            assert_eq!(Provider::parse(provider.id()), Some(provider));
        }
    }

    #[test]
    fn a_key_is_checked_against_the_providers_list_of_models() {
        assert_eq!(
            models_url(crate::claude::URL).as_deref(),
            Some("https://api.anthropic.com/v1/models")
        );
        assert_eq!(
            models_url(TOGETHER_URL).as_deref(),
            Some("https://api.together.ai/v1/models")
        );
        assert_eq!(models_url("http://127.0.0.1:9/answer"), None);
    }

    #[test]
    fn a_key_pasted_in_the_app_wins_over_the_deployments_until_removed() {
        let dir = std::env::temp_dir().join(format!("agentd-model-keys-{}", uuid::Uuid::new_v4().simple()));
        let deployment = [(Provider::Anthropic, SecretString::from("sk-ant-deployment-key"))];
        let keys = ModelKeys::open(&dir, &deployment).unwrap();
        let value = |p| keys.get(p).map(|k: SecretString| k.expose_secret().to_string());
        assert_eq!(keys.from(Provider::Anthropic), Some(KeyFrom::Deployment));
        assert_eq!(keys.from(Provider::Together), None);
        assert!(keys.put(Provider::Together, "short").is_err());

        keys.put(Provider::Anthropic, " sk-ant-pasted-in-app\n").unwrap();
        keys.put(Provider::Together, "tgp_v1_pasted-in-app").unwrap();
        assert_eq!(
            value(Provider::Anthropic).as_deref(),
            Some("sk-ant-pasted-in-app")
        );
        assert_eq!(keys.from(Provider::Anthropic), Some(KeyFrom::App));
        assert_eq!(serde_json::to_value(KeyFrom::App).unwrap(), "app");

        // Saved across a restart.
        let keys = ModelKeys::open(&dir, &deployment).unwrap();
        let value = |p| keys.get(p).map(|k: SecretString| k.expose_secret().to_string());
        assert_eq!(value(Provider::Together).as_deref(), Some("tgp_v1_pasted-in-app"));
        assert!(keys.remove(Provider::Anthropic).unwrap());
        assert_eq!(
            value(Provider::Anthropic).as_deref(),
            Some("sk-ant-deployment-key")
        );
        // The deployment's own can't be removed from the app.
        assert!(!keys.remove(Provider::Anthropic).unwrap());
        assert!(keys.has(Provider::Anthropic));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
