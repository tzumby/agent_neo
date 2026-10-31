//! Settings from KEY=VALUE files (AGENTD_CONFIG, AGENTD_SECRETS), then the environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use secrecy::SecretString;

#[derive(Clone, Debug)]
pub struct Config {
    /// `unix:/run/agentd/agentd.sock` (production) or `127.0.0.1:8765` (local dev).
    pub listen: String,
    /// Only for local development: on desktops, model calls go through the
    /// model gateway and no key is here. Never printed (Debug shows [REDACTED]).
    pub together_api_key: Option<SecretString>,
    /// The model gateway's function URL.
    pub model_gateway_url: String,
    pub model: String,
    pub cdp_url: String,
    pub state_dir: PathBuf,
    pub workdir: PathBuf,
    /// User that shell commands run as, in a hardened systemd unit. Not the
    /// desktop user: it can't reach the browser, the screen or the Chrome
    /// profile. Empty: run as agentd's own user (local development).
    pub run_as: String,
    /// Limits for each shell command (systemd MemoryMax, TasksMax, CPUQuota).
    pub shell_memory_max: String,
    pub shell_tasks_max: String,
    pub shell_cpu_quota: String,
    /// The shell user's egress proxy, e.g. http://127.0.0.1:3128 (empty: none).
    pub shell_proxy: String,
    pub policy_path: PathBuf,
    pub max_steps: u32,
    pub shell_tool: bool,
    pub desktop_name: String,
    /// The push relay's function URL; push is off without it.
    pub push_relay_url: String,
}

fn read_kv(path: &Path, into: &mut HashMap<String, String>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            into.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    Ok(())
}

impl Config {
    pub fn load() -> Result<Self> {
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let mut kv = HashMap::new();
        read_kv(
            Path::new(&env("AGENTD_CONFIG").unwrap_or("/etc/agentd/agentd.env".into())),
            &mut kv,
        )?;
        read_kv(
            Path::new(&env("AGENTD_SECRETS").unwrap_or("/etc/agentd/secrets.env".into())),
            &mut kv,
        )?;
        // The environment wins, so local runs can override the files.
        let get = |key: &str, default: &str| -> String {
            env(key)
                .or_else(|| kv.get(key).cloned())
                .unwrap_or_else(|| default.to_string())
        };
        let together_api_key = get("TOGETHER_API_KEY", "");
        let model_gateway_url = get("MODEL_GATEWAY_URL", "");
        if together_api_key.is_empty() && model_gateway_url.is_empty() {
            bail!("set MODEL_GATEWAY_URL, or TOGETHER_API_KEY for local development");
        }
        Ok(Self {
            listen: get("LISTEN", "unix:/run/agentd/agentd.sock"),
            together_api_key: (!together_api_key.is_empty()).then(|| together_api_key.into()),
            model_gateway_url,
            model: get("MODEL", "zai-org/GLM-5.3"),
            cdp_url: get("CDP_URL", "http://127.0.0.1:9222"),
            state_dir: get("STATE_DIR", "/var/lib/agentd").into(),
            workdir: get("WORKDIR", "/home/worker/work").into(),
            run_as: get("RUN_AS", "worker"),
            shell_memory_max: get("SHELL_MEMORY_MAX", "2G"),
            shell_tasks_max: get("SHELL_TASKS_MAX", "512"),
            shell_cpu_quota: get("SHELL_CPU_QUOTA", "200%"),
            shell_proxy: get("SHELL_PROXY", ""),
            policy_path: get("POLICY", "/etc/agentd/policy.toml").into(),
            max_steps: get("MAX_STEPS", "40").parse().context("MAX_STEPS")?,
            shell_tool: get("SHELL_TOOL", "on") == "on",
            desktop_name: get("DESKTOP_NAME", "cloud-desktop"),
            push_relay_url: get("PUSH_RELAY_URL", ""),
        })
    }
}
