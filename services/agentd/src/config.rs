//! Settings from KEY=VALUE files (AGENTD_CONFIG, AGENTD_SECRETS), then the environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use secrecy::SecretString;

#[derive(Clone, Debug)]
pub struct Config {
    /// `unix:/run/agentd/agentd.sock` (production) or `127.0.0.1:8765` (local dev).
    pub listen: String,
    /// The model provider's key: LLM_API_KEY, else TOGETHER_API_KEY. On a
    /// desktop it's in AGENTD_SECRETS (/etc/agentd/secrets.env, root:agentd
    /// 0640, written by Ansible from SSM), which the shell user can't read.
    /// Never printed (Debug shows [REDACTED]).
    pub api_key: SecretString,
    /// An OpenAI-compatible chat completions URL (default Together's; the
    /// evals can use Bedrock's).
    pub llm_url: String,
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
    /// The shell user's egress proxy, e.g. http://127.0.0.1:3128 (empty: none).
    pub shell_proxy: String,
    /// The file of sites the user allowed, which Squid reads (access.rs);
    /// empty: the agent can't ask for new sites on this desktop.
    pub egress_user_hosts: String,
    /// The deployment's list (Ansible's), as Squid has it: shown in Settings
    /// and checked before asking.
    pub egress_base_hosts: Vec<String>,
    pub policy_path: PathBuf,
    /// Skills: Markdown how-tos the agent reads on demand (skills.rs).
    pub skills_dir: PathBuf,
    /// The supervisor checks on a running task after this much working time
    /// (seconds) or this many model turns, whichever comes first (supervise.rs).
    /// There's no cap on steps: a task runs until it's done, is stopped, or the
    /// supervisor finds it stuck. 0 turns that trigger off.
    pub review_every_secs: u64,
    pub review_every_steps: u32,
    /// Notifications (notify.rs): the contact push services see in each
    /// request's VAPID token (a mailto: or https: URL).
    pub vapid_subject: String,
    /// Local development only: subscriptions may point at http://127.0.0.1.
    pub push_dev_endpoints: bool,
    /// Notifications to the iPhone and Mac apps (apns.rs): your Apple APNs auth
    /// key (a `.p8`, as PEM or that PEM base64-encoded) and the ids that name
    /// it. All three set turns the apps' notifications on; empty leaves them
    /// off (the apps still work, the browsers still get Web Push). Kept in
    /// /etc/agentd/secrets.env beside the model key.
    pub apns_auth_key: String,
    pub apns_key_id: String,
    pub apns_team_id: String,
    pub shell_tool: bool,
    pub desktop_name: String,
    /// Memory v2 (docs/memory-v2.md): kinds, trust, retrieval, consolidation.
    /// Off: the v1 memory path, on the v2 schema (which is always migrated).
    pub memory_v2: bool,
    /// How many conversations may work at the same moment (sessions.rs);
    /// more messages wait in the queue. At least 1.
    pub max_parallel_tasks: usize,
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
        if api_key.is_empty() {
            bail!("set TOGETHER_API_KEY (or LLM_API_KEY) in /etc/agentd/secrets.env or the environment");
        }
        Ok(Self {
            listen: get("LISTEN", "unix:/run/agentd/agentd.sock"),
            api_key: api_key.into(),
            llm_url: get("LLM_URL", crate::llm::TOGETHER_URL),
            model: get("MODEL", "zai-org/GLM-5.3"),
            consolidation_model: get("CONSOLIDATION_MODEL", crate::consolidate::DEFAULT_MODEL),
            cdp_url: get("CDP_URL", "http://127.0.0.1:9222"),
            state_dir: get("STATE_DIR", "/var/lib/agentd").into(),
            workdir: get("WORKDIR", "/home/worker/work").into(),
            run_as: std::env::var("RUN_AS").unwrap_or_else(|_| get("RUN_AS", "worker")),
            shell_proxy: get("SHELL_PROXY", ""),
            egress_user_hosts: get("EGRESS_USER_HOSTS", ""),
            egress_base_hosts: get("EGRESS_BASE_HOSTS", "")
                .split_whitespace()
                .map(str::to_ascii_lowercase)
                .collect(),
            policy_path: get("POLICY", "/etc/agentd/policy.toml").into(),
            skills_dir: get("SKILLS_DIR", "/etc/agentd/skills").into(),
            review_every_secs: get("REVIEW_EVERY_SECS", "1800")
                .parse()
                .context("REVIEW_EVERY_SECS")?,
            review_every_steps: get("REVIEW_EVERY_STEPS", "60")
                .parse()
                .context("REVIEW_EVERY_STEPS")?,
            vapid_subject: get("VAPID_SUBJECT", "mailto:neo-agent@example.com"),
            push_dev_endpoints: get("PUSH_DEV_ENDPOINTS", "off") == "on",
            apns_auth_key: get("APNS_AUTH_KEY", ""),
            apns_key_id: get("APNS_KEY_ID", ""),
            apns_team_id: get("APNS_TEAM_ID", ""),
            shell_tool: get("SHELL_TOOL", "on") == "on",
            desktop_name: get("DESKTOP_NAME", "neo-agent"),
            memory_v2: get("MEMORY_V2", "off") == "on",
            max_parallel_tasks: get("MAX_PARALLEL_TASKS", "3")
                .parse::<usize>()
                .context("MAX_PARALLEL_TASKS")?
                .max(1),
        })
    }
}
