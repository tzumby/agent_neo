//! Runs the agent's shell commands as the desktop user (never as agentd, so
//! they can't read its secrets or state), or as root once the user approved.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;

use crate::llm::truncate;

pub struct Shell {
    /// Empty: run as agentd's own user (local development only).
    pub run_as: String,
    pub workdir: PathBuf,
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `sudo apt install jq` → (true, "apt install jq").
pub fn split_sudo(command: &str) -> (bool, &str) {
    match command.trim_start().strip_prefix("sudo ") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, command),
    }
}

impl Shell {
    pub async fn run(&self, command: &str, as_root: bool, timeout_secs: u64) -> Result<String> {
        let timeout_secs = timeout_secs.clamp(1, 900);
        let script = format!("cd {} && {command}", quote(&self.workdir.to_string_lossy()));
        let local = !as_root && self.run_as.is_empty();
        let secs = timeout_secs.to_string();
        let mut argv: Vec<&str> = Vec::new();
        if as_root {
            argv.extend(["sudo", "-n", "--"]);
        } else if !local {
            argv.extend(["sudo", "-n", "-u", &self.run_as, "-H", "--"]);
        }
        if !local {
            // Runs as the target user, so it can kill the whole command tree.
            argv.extend(["timeout", "-k", "5", &secs]);
        }
        argv.extend(["bash", "-lc", &script]);

        let mut command = tokio::process::Command::new(argv[0]);
        command.args(&argv[1..]).stdin(Stdio::null()).kill_on_drop(true);
        if local {
            command
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin");
            if let Ok(home) = std::env::var("HOME") {
                command.env("HOME", home);
            }
        }
        let output =
            match tokio::time::timeout(Duration::from_secs(timeout_secs + 15), command.output()).await {
                Err(_) => return Ok(format!("error: timed out after {timeout_secs}s")),
                Ok(output) => output?,
            };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let code = output.status.code();
        let note = if code == Some(124) && !local {
            " (timed out)"
        } else {
            ""
        };
        let code = code.map_or("?".to_string(), |c| c.to_string());
        Ok(format!("exit code {code}{note}\n{}", truncate(text.trim(), 8000)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudo_prefix() {
        assert_eq!(split_sudo("sudo apt install -y jq"), (true, "apt install -y jq"));
        assert_eq!(split_sudo("  sudo  ls"), (true, "ls"));
        assert_eq!(split_sudo("echo sudo"), (false, "echo sudo"));
    }

    #[tokio::test]
    async fn runs_locally_in_the_workdir() {
        let shell = Shell {
            run_as: String::new(),
            workdir: std::env::temp_dir(),
        };
        let out = shell.run("echo hi && pwd", false, 10).await.unwrap();
        assert!(out.starts_with("exit code 0\nhi"), "{out}");
        let out = shell.run("exit 3", false, 10).await.unwrap();
        assert!(out.starts_with("exit code 3"), "{out}");
    }
}
