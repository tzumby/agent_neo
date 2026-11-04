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
    /// LLM_API_KEY, else TOGETHER_API_KEY.
    pub api_key: Option<SecretString>,
    /// Only for local development, with the key above: an OpenAI-compatible
    /// chat completions URL (default Together's; the evals use Bedrock's).
    /// Ignored when GATEWAY_URL is set: the gateway is then the only way out.
    pub llm_url: String,
    /// The attested model gateway (https://<address>:8443). When set, model
    /// calls go only there, after its attestation checks out.
    pub gateway_url: String,
    /// The gateway images agentd accepts (attest.rs: parse_measurements).
    pub gateway_measurements: Vec<crate::attest::Measurements>,
    /// Signed into this desktop's identity proofs for the gateway.
    pub gateway_audience: String,
    /// For the identity proof (STS in this region).
    pub aws_region: String,
    pub model: String,
    /// Memory tidy-up's model when the user hasn't picked one.
    pub consolidation_model: String,
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
    /// The egress gate (egress.rs), scheduled runs' way out: where it listens
    /// (host:port on localhost; empty: not started, and scheduled commands go
    /// out like any other) and where it sends traffic on (Squid, host:port).
    pub egress_gate_listen: String,
    pub egress_gate_squid: String,
    /// Local development and tests only: the gate sends opened-up requests
    /// to `{this}/{host}{path}` instead of the real hosts.
    pub egress_gate_upstream: Option<String>,
    pub policy_path: PathBuf,
    /// Skills: Markdown how-tos the agent reads on demand (skills.rs).
    pub skills_dir: PathBuf,
    pub max_steps: u32,
    pub shell_tool: bool,
    pub desktop_name: String,
    /// The push relay's function URL; push is off without it.
    pub push_relay_url: String,
    /// Memory v2 (docs/memory-v2.md): kinds, trust, retrieval, consolidation.
    /// Off: the v1 memory path, on the v2 schema (which is always migrated).
    pub memory_v2: bool,
    /// Slack's Web API (destinations.rs). Local development only points it
    /// at a stand-in; plain http is then allowed for posts.
    pub slack_api: String,
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
        let api_key = Some(get("LLM_API_KEY", ""))
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| get("TOGETHER_API_KEY", ""));
        let gateway_url = get("GATEWAY_URL", "");
        if api_key.is_empty() && gateway_url.is_empty() {
            bail!("set GATEWAY_URL (or TOGETHER_API_KEY for local development)");
        }
        let gateway_measurements = if gateway_url.is_empty() {
            Vec::new()
        } else {
            crate::attest::parse_measurements(&get("GATEWAY_MEASUREMENTS", ""))
                .context("GATEWAY_MEASUREMENTS")?
        };
        Ok(Self {
            listen: get("LISTEN", "unix:/run/agentd/agentd.sock"),
            api_key: (!api_key.is_empty()).then(|| api_key.into()),
            llm_url: get("LLM_URL", crate::llm::TOGETHER_URL),
            gateway_url,
            gateway_measurements,
            gateway_audience: get("GATEWAY_AUDIENCE", "cloud-desktops/model-gateway"),
            aws_region: get("AWS_REGION", "ca-central-1"),
            model: get("MODEL", "zai-org/GLM-5.3"),
            consolidation_model: get("CONSOLIDATION_MODEL", crate::consolidate::DEFAULT_MODEL),
            cdp_url: get("CDP_URL", "http://127.0.0.1:9222"),
            state_dir: get("STATE_DIR", "/var/lib/agentd").into(),
            workdir: get("WORKDIR", "/home/worker/work").into(),
            run_as: get("RUN_AS", "worker"),
            shell_memory_max: get("SHELL_MEMORY_MAX", "2G"),
            shell_tasks_max: get("SHELL_TASKS_MAX", "512"),
            shell_cpu_quota: get("SHELL_CPU_QUOTA", "200%"),
            shell_proxy: get("SHELL_PROXY", ""),
            egress_gate_listen: get("EGRESS_GATE_LISTEN", ""),
            egress_gate_squid: get("EGRESS_GATE_SQUID", "127.0.0.1:3128"),
            egress_gate_upstream: Some(get("EGRESS_GATE_UPSTREAM", "")).filter(|u| !u.is_empty()),
            policy_path: get("POLICY", "/etc/agentd/policy.toml").into(),
            skills_dir: get("SKILLS_DIR", "/etc/agentd/skills").into(),
            max_steps: get("MAX_STEPS", "40").parse().context("MAX_STEPS")?,
            shell_tool: get("SHELL_TOOL", "on") == "on",
            desktop_name: get("DESKTOP_NAME", "cloud-desktop"),
            push_relay_url: get("PUSH_RELAY_URL", ""),
            memory_v2: get("MEMORY_V2", "off") == "on",
            slack_api: get("SLACK_API_URL", "https://slack.com/api"),
        })
    }
}
