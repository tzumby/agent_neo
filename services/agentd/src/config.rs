//! Settings from KEY=VALUE files (AGENTD_CONFIG, AGENTD_SECRETS), then the environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct Config {
    /// `unix:/run/agentd/agentd.sock` (production) or `127.0.0.1:8765` (local dev).
    pub listen: String,
    pub together_api_key: String,
    pub model: String,
    pub cdp_url: String,
    pub state_dir: PathBuf,
    pub workdir: PathBuf,
    /// User that shell commands run as (via sudo). Empty: run as agentd's own user (dev).
    pub run_as: String,
    pub policy_path: PathBuf,
    pub max_steps: u32,
    pub shell_tool: bool,
    pub desktop_name: String,
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
        if together_api_key.is_empty() {
            bail!("TOGETHER_API_KEY is not set (AGENTD_SECRETS file or environment)");
        }
        Ok(Self {
            listen: get("LISTEN", "unix:/run/agentd/agentd.sock"),
            together_api_key,
            model: get("MODEL", "zai-org/GLM-5.3"),
            cdp_url: get("CDP_URL", "http://127.0.0.1:9222"),
            state_dir: get("STATE_DIR", "/var/lib/agentd").into(),
            workdir: get("WORKDIR", "/home/agent/work").into(),
            run_as: get("RUN_AS", "agent"),
            policy_path: get("POLICY", "/etc/agentd/policy.toml").into(),
            max_steps: get("MAX_STEPS", "40").parse().context("MAX_STEPS")?,
            shell_tool: get("SHELL_TOOL", "on") == "on",
            desktop_name: get("DESKTOP_NAME", "cloud-desktop"),
        })
    }
}
