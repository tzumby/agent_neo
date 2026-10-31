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

/// What a command printed (stdout, then stderr) and how it ended.
pub struct ShellOutput {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub text: String,
    limit_secs: u64,
}

impl ShellOutput {
    /// For the model: the exit code, then the start of the output.
    pub fn for_model(&self) -> String {
        if self.timed_out && self.code.is_none() {
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

/// `sudo apt install jq` → (true, "apt install jq").
pub fn split_sudo(command: &str) -> (bool, &str) {
    match command.trim_start().strip_prefix("sudo ") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, command),
    }
}

impl Shell {
    pub async fn run(&self, command: &str, as_root: bool, timeout_secs: u64) -> Result<ShellOutput> {
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
                Err(_) => {
                    return Ok(ShellOutput {
                        code: None,
                        timed_out: true,
                        text: String::new(),
                        limit_secs: timeout_secs,
                    });
                }
                Ok(output) => output?,
            };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let code = output.status.code();
        Ok(ShellOutput {
            code,
            timed_out: code == Some(124) && !local,
            text: text.trim().to_string(),
            limit_secs: timeout_secs,
        })
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
        assert!(
            out.for_model().starts_with("exit code 0\nhi"),
            "{}",
            out.for_model()
        );
        let out = shell.run("exit 3", false, 10).await.unwrap();
        assert_eq!(out.code, Some(3));
        assert!(out.for_model().starts_with("exit code 3"), "{}", out.for_model());
    }

    #[test]
    fn tail_keeps_the_end() {
        let out = ShellOutput {
            code: Some(0),
            timed_out: false,
            text: "abcdef".into(),
            limit_secs: 1,
        };
        assert_eq!(out.tail(10), "abcdef");
        assert_eq!(out.tail(2), "[4 earlier characters]\nef");
    }
}
