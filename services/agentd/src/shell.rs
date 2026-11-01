//! Runs the agent's shell commands, each in its own transient systemd unit.
//!
//! Commands run as the shell user (`worker`): not agentd, so they can't read
//! agentd's secrets or state, and not the desktop user, so they can't reach
//! the browser, the screen or the Chrome profile (a firewall rule and the X
//! cookie keep them out). Commands starting with `sudo` run as root, only
//! after the user approved them.
//!
//! The unit's hardening follows moltis' container settings (MIT License,
//! Copyright (c) the moltis contributors; crates/tools/src/sandbox/docker.rs):
//! no privilege escalation, no capabilities, a private /tmp, and limits on
//! memory, processes, CPU and run time. Stopping the unit kills everything the
//! command started.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::watch;

use crate::llm::truncate;

pub struct Shell {
    /// Empty: run as agentd's own user, directly (local development only).
    pub run_as: String,
    pub workdir: PathBuf,
    pub memory_max: String,
    pub tasks_max: String,
    pub cpu_quota: String,
    /// The guarded egress proxy for the shell user (empty: none). The firewall
    /// lets the shell user out only through it; it allows listed hosts, refuses
    /// internal addresses and logs every request.
    pub proxy: String,
}

/// What a command printed (stdout, then stderr) and how it ended.
pub struct ShellOutput {
    pub code: Option<i32>,
    pub timed_out: bool,
    /// The user pressed Stop while it ran.
    pub stopped: bool,
    pub text: String,
    limit_secs: u64,
}

impl ShellOutput {
    /// For the model: the exit code, then the start of the output.
    pub fn for_model(&self) -> String {
        if self.stopped {
            return "stopped: the user stopped the task while this command ran".into();
        }
        if self.timed_out && self.text.is_empty() {
            return format!("error: timed out after {}s", self.limit_secs);
        }
        let code = self.code.map_or("?".to_string(), |c| c.to_string());
        let note = if self.timed_out { " (timed out)" } else { "" };
        format!("exit code {code}{note}\n{}", truncate(&self.text, 8000))
    }

    /// For the app: the end of the output, which usually matters most.
    pub fn tail(&self, max_chars: usize) -> String {
        let count = self.text.chars().count();
        if count <= max_chars {
            return self.text.clone();
        }
        let kept: String = self.text.chars().skip(count - max_chars).collect();
        format!("[{} earlier characters]\n{kept}", count - max_chars)
    }
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// A saved key a command uses: exported as `name`, read from `path`.
pub struct KeyFile {
    pub name: String,
    pub path: PathBuf,
}

/// Exports each key from the unit's credentials, then runs the command.
fn with_keys(command: &str, keys: &[KeyFile]) -> String {
    if keys.is_empty() {
        return command.to_string();
    }
    let mut script: String = keys
        .iter()
        .map(|key| format!("export {0}=\"$(< \"$CREDENTIALS_DIRECTORY/{0}\")\"\n", key.name))
        .collect();
    script.push_str(command);
    script
}

/// `sudo apt install jq` → (true, "apt install jq").
pub fn split_sudo(command: &str) -> (bool, &str) {
    match command.trim_start().strip_prefix("sudo ") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, command),
    }
}

/// Hardening for the shell user's commands (not for approved root commands,
/// which may need to change the system).
const HARDENING: &[&str] = &[
    "NoNewPrivileges=yes",
    "CapabilityBoundingSet=",
    "AmbientCapabilities=",
    "PrivateTmp=yes",
    "ProtectSystem=full",
    "ProtectKernelTunables=yes",
    "ProtectKernelModules=yes",
    "ProtectKernelLogs=yes",
    "ProtectControlGroups=yes",
    "ProtectClock=yes",
    "ProtectHostname=yes",
    "RestrictSUIDSGID=yes",
    "RestrictRealtime=yes",
    "LockPersonality=yes",
];

impl Shell {
    fn local(&self, as_root: bool) -> bool {
        !as_root && self.run_as.is_empty()
    }

    /// The command line: a transient unit through `sudo systemd-run`, or a
    /// plain `bash` in local development.
    fn argv(
        &self,
        unit: &str,
        command: &str,
        as_root: bool,
        timeout_secs: u64,
        keys: &[KeyFile],
    ) -> Vec<String> {
        let workdir = self.workdir.to_string_lossy();
        if self.local(as_root) {
            // Keys come through the environment here (run()).
            let script = format!("cd {} && {command}", quote(&workdir));
            return vec!["bash".into(), "-lc".into(), script];
        }
        let mut argv: Vec<String> = [
            "sudo",
            "-n",
            "--",
            "systemd-run",
            "--quiet",
            "--pipe",
            "--wait",
            "--collect",
            "--service-type=exec",
        ]
        .map(String::from)
        .to_vec();
        argv.push(format!("--unit={unit}"));
        argv.push(format!("--working-directory={workdir}"));
        argv.push(format!("--property=RuntimeMaxSec={timeout_secs}"));
        if as_root {
            argv.push("--setenv=HOME=/root".into());
        } else {
            argv.push(format!("--uid={}", self.run_as));
            for property in HARDENING {
                argv.push(format!("--property={property}"));
            }
            if !self.proxy.is_empty() {
                for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                    argv.push(format!("--setenv={name}={}", self.proxy));
                }
                argv.push("--setenv=NO_PROXY=localhost,127.0.0.1,::1".into());
                argv.push("--setenv=no_proxy=localhost,127.0.0.1,::1".into());
            }
            argv.push(format!("--property=MemoryMax={}", self.memory_max));
            argv.push(format!("--property=TasksMax={}", self.tasks_max));
            argv.push(format!("--property=CPUQuota={}", self.cpu_quota));
        }
        for key in keys {
            argv.push(format!(
                "--property=LoadCredential={}:{}",
                key.name,
                key.path.display()
            ));
        }
        argv.extend(["--", "bash", "-lc"].map(String::from));
        argv.push(with_keys(command, keys));
        argv
    }

    pub async fn run(
        &self,
        command: &str,
        as_root: bool,
        timeout_secs: u64,
        keys: &[KeyFile],
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<ShellOutput> {
        let timeout_secs = timeout_secs.clamp(1, 900);
        let unit = format!("agentd-cmd-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let argv = self.argv(&unit, command, as_root, timeout_secs, keys);
        let local = self.local(as_root);

        let mut process = tokio::process::Command::new(&argv[0]);
        process
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if local {
            process
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin");
            if let Ok(home) = std::env::var("HOME") {
                process.env("HOME", home);
            }
            for key in keys {
                let value = std::fs::read_to_string(&key.path)?;
                process.env(&key.name, value.trim());
            }
        }
        let started = Instant::now();
        let child = process.spawn()?;
        let limit = Duration::from_secs(timeout_secs + 15);
        let ended = tokio::select! {
            output = tokio::time::timeout(limit, child.wait_with_output()) => output.ok(),
            _ = cancel.wait_for(|stopped| *stopped) => None,
        };
        let Some(output) = ended else {
            // Stop (or our backstop timeout): stop the unit, which kills
            // everything the command started.
            if !local {
                let _ = tokio::process::Command::new("sudo")
                    .args(["-n", "--", "systemctl", "stop", &unit])
                    .stdin(Stdio::null())
                    .status()
                    .await;
            }
            let stopped = *cancel.borrow();
            return Ok(ShellOutput {
                code: None,
                timed_out: !stopped,
                stopped,
                text: String::new(),
                limit_secs: timeout_secs,
            });
        };
        let output = output?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(ShellOutput {
            code: output.status.code(),
            // RuntimeMaxSec ended the unit.
            timed_out: !local && started.elapsed() >= Duration::from_secs(timeout_secs),
            stopped: false,
            text: text.trim().to_string(),
            limit_secs: timeout_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(run_as: &str) -> Shell {
        Shell {
            run_as: run_as.into(),
            workdir: "/home/worker/work".into(),
            memory_max: "2G".into(),
            tasks_max: "512".into(),
            cpu_quota: "200%".into(),
            proxy: "http://127.0.0.1:3128".into(),
        }
    }

    #[test]
    fn sudo_prefix() {
        assert_eq!(split_sudo("sudo apt install -y jq"), (true, "apt install -y jq"));
        assert_eq!(split_sudo("  sudo  ls"), (true, "ls"));
        assert_eq!(split_sudo("echo sudo"), (false, "echo sudo"));
    }

    #[test]
    fn worker_commands_run_in_a_hardened_unit() {
        let argv = shell("worker").argv("agentd-cmd-x", "ls -la", false, 60, &[]);
        let has = |s: &str| argv.iter().any(|a| a == s);
        assert_eq!(&argv[..4], ["sudo", "-n", "--", "systemd-run"]);
        assert!(has("--uid=worker") && has("--unit=agentd-cmd-x"));
        assert!(has("--working-directory=/home/worker/work"));
        assert!(has("--property=NoNewPrivileges=yes") && has("--property=PrivateTmp=yes"));
        assert!(has("--property=MemoryMax=2G") && has("--property=RuntimeMaxSec=60"));
        assert!(
            has("--setenv=HTTPS_PROXY=http://127.0.0.1:3128")
                && has("--setenv=NO_PROXY=localhost,127.0.0.1,::1")
        );
        assert_eq!(&argv[argv.len() - 4..], ["--", "bash", "-lc", "ls -la"]);
    }

    #[test]
    fn approved_root_commands_run_as_root_without_the_shell_limits() {
        let argv = shell("worker").argv("agentd-cmd-y", "apt install -y jq", true, 300, &[]);
        assert!(
            !argv
                .iter()
                .any(|a| a.starts_with("--uid") || a.contains("NoNewPrivileges") || a.contains("PROXY"))
        );
        assert!(argv.iter().any(|a| a == "--setenv=HOME=/root"));
        assert_eq!(argv.last().unwrap(), "apt install -y jq");
    }

    #[test]
    fn keys_come_as_credentials_never_on_the_command_line() {
        let keys = [KeyFile {
            name: "GITHUB_TOKEN".into(),
            path: "/var/lib/agentd/secrets/GITHUB_TOKEN".into(),
        }];
        let argv = shell("worker").argv("agentd-cmd-z", "gh repo clone acme/api", false, 60, &keys);
        assert!(
            argv.iter()
                .any(|a| a == "--property=LoadCredential=GITHUB_TOKEN:/var/lib/agentd/secrets/GITHUB_TOKEN")
        );
        assert_eq!(
            argv.last().unwrap(),
            "export GITHUB_TOKEN=\"$(< \"$CREDENTIALS_DIRECTORY/GITHUB_TOKEN\")\"\ngh repo clone acme/api"
        );
    }

    #[tokio::test]
    async fn local_commands_get_keys_in_the_environment() {
        let dir = std::env::temp_dir().join(format!("agentd-key-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("API_KEY"), "abc123456789\n").unwrap();
        let shell = Shell {
            workdir: std::env::temp_dir(),
            ..shell("")
        };
        let keys = [KeyFile {
            name: "API_KEY".into(),
            path: dir.join("API_KEY"),
        }];
        let (_stop, mut cancel) = watch::channel(false);
        let out = shell
            .run("echo \"[$API_KEY]\"", false, 10, &keys, &mut cancel)
            .await
            .unwrap();
        assert_eq!(out.text, "[abc123456789]");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn runs_locally_in_the_workdir() {
        let shell = Shell {
            workdir: std::env::temp_dir(),
            ..shell("")
        };
        let (_stop, mut cancel) = watch::channel(false);
        let out = shell
            .run("echo hi && pwd", false, 10, &[], &mut cancel)
            .await
            .unwrap();
        assert!(
            out.for_model().starts_with("exit code 0\nhi"),
            "{}",
            out.for_model()
        );
        let out = shell.run("exit 3", false, 10, &[], &mut cancel).await.unwrap();
        assert_eq!(out.code, Some(3));
    }

    #[tokio::test]
    async fn stop_ends_a_running_command() {
        let shell = Shell {
            workdir: std::env::temp_dir(),
            ..shell("")
        };
        let (stop, mut cancel) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = stop.send(true);
        });
        let started = Instant::now();
        let out = shell.run("sleep 30", false, 60, &[], &mut cancel).await.unwrap();
        assert!(out.stopped && started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn tail_keeps_the_end() {
        let out = ShellOutput {
            code: Some(0),
            timed_out: false,
            stopped: false,
            text: "abcdef".into(),
            limit_secs: 1,
        };
        assert_eq!(out.tail(10), "abcdef");
        assert_eq!(out.tail(2), "[4 earlier characters]\nef");
    }
}
