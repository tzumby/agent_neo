//! Runs the agent's shell commands, each in its own transient systemd unit.
//!
//! Commands run as the shell user (`worker`): not agentd, so they can't read
//! agentd's secrets or state, and not the desktop user, so they can't reach
//! the browser, the screen or the Chrome profile. The root-owned launcher only
//! starts unprivileged commands with fixed isolation and resource limits.
//!
//! Each command has a `Route` to the internet: through Squid, or (one the
//! user approved) directly.
//!
//! The unit's hardening follows moltis' container settings (MIT License,
//! Copyright (c) the moltis contributors; crates/tools/src/sandbox/docker.rs):
//! no privilege escalation, no capabilities, a private /tmp, and limits on
//! memory, processes, CPU and run time. Stopping the unit kills everything the
//! command started.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::watch;

use crate::llm::truncate;

pub struct Shell {
    /// Empty: run as agentd's own user, directly (local development only).
    pub run_as: String,
    pub workdir: PathBuf,
}

pub const PRIVILEGED_HELPER: &str = "/usr/local/libexec/neo-agent-privileged";
const OUTPUT_HEAD: usize = 16 * 1024;
const OUTPUT_TAIL: usize = 8 * 1024;

/// The longest a command may run.
pub const MAX_COMMAND_SECS: u64 = 900;

/// How a shell user's command reaches the internet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Route {
    /// Through Squid, the usual way.
    Proxy,
    /// An explicitly approved command using the direct egress slice.
    Direct,
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

/// `sudo apt install jq` → (true, "apt install jq").
pub fn split_sudo(command: &str) -> (bool, &str) {
    match command.trim_start().strip_prefix("sudo ") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, command),
    }
}

/// Captured beyond each cut: at least the longest saved value
/// (secrets::MAX_LEN), so a value that starts before a cut is whole in the
/// buffer, and is redacted, not split, when the output is cut to size.
const MARGIN: usize = crate::secrets::MAX_LEN;

/// One stream's output, bounded while the command runs.
#[derive(Default)]
struct Captured {
    /// The first OUTPUT_HEAD + MARGIN bytes.
    head: Vec<u8>,
    /// The last OUTPUT_TAIL + MARGIN bytes after those.
    tail: VecDeque<u8>,
    /// Bytes dropped between the two.
    omitted: usize,
}

impl Captured {
    fn push(&mut self, mut chunk: &[u8]) {
        let head_bytes = chunk.len().min(OUTPUT_HEAD + MARGIN - self.head.len());
        self.head.extend_from_slice(&chunk[..head_bytes]);
        chunk = &chunk[head_bytes..];
        let overflow = (self.tail.len() + chunk.len()).saturating_sub(OUTPUT_TAIL + MARGIN);
        self.omitted += overflow;
        let removed = overflow.min(self.tail.len());
        self.tail.drain(..removed);
        self.tail.extend(&chunk[overflow - removed..]);
    }

    /// The output with `values` (name, value; longest first) redacted, cut to
    /// about OUTPUT_HEAD bytes from the start and OUTPUT_TAIL from the end.
    /// Values are found before cutting, and a cut inside one moves to its edge,
    /// so no part of a value is left at a cut. The margins make sure a value
    /// that runs past the captured bytes lies beyond the cuts.
    fn text(mut self, values: &[(String, String)]) -> String {
        if self.omitted == 0 {
            self.head.extend(self.tail);
            let all = String::from_utf8_lossy(&self.head);
            if all.len() <= OUTPUT_HEAD + OUTPUT_TAIL {
                return redact(&all, values);
            }
            let spans = spans(&all, values);
            let head_end = head_cut(&all, OUTPUT_HEAD, &spans);
            let tail_start = tail_cut(&all, all.len() - OUTPUT_TAIL, &spans).max(head_end);
            return cut_text(
                &all[..head_end],
                tail_start - head_end,
                &all[tail_start..],
                values,
            );
        }
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(self.tail.make_contiguous());
        let head_end = head_cut(&head, OUTPUT_HEAD, &spans(&head, values));
        let tail_start = tail_cut(
            &tail,
            tail.len().saturating_sub(OUTPUT_TAIL),
            &spans(&tail, values),
        );
        let omitted = head.len() - head_end + self.omitted + tail_start;
        cut_text(&head[..head_end], omitted, &tail[tail_start..], values)
    }
}

/// Where `values` occur in `text`, as merged, sorted byte ranges.
fn spans(text: &str, values: &[(String, String)]) -> Vec<(usize, usize)> {
    let mut found: Vec<(usize, usize)> = values
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .flat_map(|(_, value)| {
            text.match_indices(value.as_str())
                .map(|(start, value)| (start, start + value.len()))
        })
        .collect();
    found.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(found.len());
    for (start, end) in found {
        match merged.last_mut() {
            Some(last) if start < last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The end of the kept start: at most `at`, never inside a value.
fn head_cut(text: &str, at: usize, spans: &[(usize, usize)]) -> usize {
    let mut cut = at.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    for &(start, end) in spans {
        if start < cut && cut < end {
            cut = start;
        }
    }
    cut
}

/// The start of the kept end: at least `at`, never inside a value.
fn tail_cut(text: &str, at: usize, spans: &[(usize, usize)]) -> usize {
    let mut cut = at.min(text.len());
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    for &(start, end) in spans {
        if start < cut && cut < end {
            cut = end;
        }
    }
    cut
}

fn cut_text(head: &str, omitted: usize, tail: &str, values: &[(String, String)]) -> String {
    format!(
        "{}\n[{omitted} output bytes omitted]\n{}",
        redact(head, values),
        redact(tail, values)
    )
}

/// Each value (longest first) replaced by `[secret NAME]`.
fn redact(text: &str, values: &[(String, String)]) -> String {
    let mut text = text.to_string();
    for (name, value) in values {
        if !value.is_empty() && text.contains(value.as_str()) {
            text = text.replace(value.as_str(), &format!("[secret {name}]"));
        }
    }
    text
}

async fn capture(mut pipe: impl AsyncRead + Unpin) -> std::io::Result<Captured> {
    let mut captured = Captured::default();
    let mut chunk = [0; 8192];
    loop {
        let count = pipe.read(&mut chunk).await?;
        if count == 0 {
            return Ok(captured);
        }
        captured.push(&chunk[..count]);
    }
}

fn privileged(action: &str) -> Command {
    let mut process = Command::new("/usr/bin/sudo");
    process.args(["-n", "--", PRIVILEGED_HELPER, action]);
    process
}

async fn send_request(stdin: Option<ChildStdin>, request: &[u8]) -> std::io::Result<()> {
    if let Some(mut stdin) = stdin {
        stdin.write_all(request).await?;
    }
    Ok(())
}

async fn collect(mut child: Child, request: &[u8]) -> Result<(ExitStatus, Captured, Captured)> {
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("capturing stdout")?;
    let stderr = child.stderr.take().context("capturing stderr")?;
    let (sent, status, stdout, stderr) = tokio::join!(
        send_request(stdin, request),
        child.wait(),
        capture(stdout),
        capture(stderr)
    );
    sent.context("sending the worker request")?;
    Ok((status?, stdout?, stderr?))
}

async fn stop_worker(unit: &str) -> Result<()> {
    let mut process = privileged("stop-worker");
    let mut child = process.stdin(Stdio::piped()).stdout(Stdio::null()).spawn()?;
    send_request(child.stdin.take(), unit.as_bytes()).await?;
    anyhow::ensure!(child.wait().await?.success(), "worker unit did not stop");
    Ok(())
}

impl Shell {
    /// `saved`: every saved key's (name, value), redacted from the output
    /// before it's cut to size, along with the values `keys` gave this command.
    pub async fn run(
        &self,
        command: &str,
        timeout_secs: u64,
        keys: &[KeyFile],
        saved: &[(String, String)],
        route: Route,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<ShellOutput> {
        let timeout_secs = timeout_secs.clamp(1, MAX_COMMAND_SECS);
        let unit = format!("agentd-cmd-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let local = self.run_as.is_empty();
        let key_values = keys
            .iter()
            .map(|key| std::fs::read_to_string(&key.path).map(|value| (&key.name, value.trim().to_string())))
            .collect::<std::io::Result<BTreeMap<_, _>>>()?;
        let (mut process, request) = if local {
            let mut process = Command::new("/bin/bash");
            process
                .args(["--noprofile", "--norc", "-c"])
                .arg(format!(
                    "cd {} && {command}",
                    quote(&self.workdir.to_string_lossy())
                ))
                .stdin(Stdio::null())
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin")
                .envs(&key_values);
            if let Ok(home) = std::env::var("HOME") {
                process.env("HOME", home);
            }
            (process, Vec::new())
        } else {
            let mut process = privileged("run-worker");
            process.stdin(Stdio::piped());
            let route = match route {
                Route::Proxy => "proxy",
                Route::Direct => "direct",
            };
            let request = serde_json::to_vec(&json!({
                "unit": unit, "command": command, "seconds": timeout_secs, "route": route, "keys": key_values,
            }))?;
            (process, request)
        };
        process
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let started = Instant::now();
        let child = process.spawn()?;
        let limit = Duration::from_secs(timeout_secs + 15);
        let ended = tokio::select! {
            output = tokio::time::timeout(limit, collect(child, &request)) => output.ok(),
            _ = cancel.wait_for(|stopped| *stopped) => None,
        };
        let Some(output) = ended else {
            // Stop (or our backstop timeout): stop the entire unit tree.
            if !local {
                match tokio::time::timeout(Duration::from_secs(30), stop_worker(&unit)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => tracing::error!(%err, %unit, "couldn't stop the worker command"),
                    Err(_) => tracing::error!(%unit, "timed out waiting for the worker to stop"),
                }
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
        let (status, stdout, stderr) = output?;
        // The values this command received (still redacted if a key was
        // rotated while it ran) and every saved one, longest first.
        let mut values: Vec<(String, String)> = key_values
            .iter()
            .map(|(name, value)| ((*name).clone(), value.clone()))
            .chain(saved.iter().cloned())
            .filter(|(_, value)| !value.is_empty())
            .collect();
        values.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.1.cmp(&b.1)));
        values.dedup_by(|a, b| a.1 == b.1);
        let mut text = stdout.text(&values);
        text.push_str(&stderr.text(&values));
        let text = text.trim().to_string();
        Ok(ShellOutput {
            code: status.code(),
            timed_out: !local && started.elapsed() >= Duration::from_secs(timeout_secs),
            stopped: false,
            text,
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
        }
    }

    #[test]
    fn sudo_prefix() {
        assert_eq!(split_sudo("sudo apt install -y jq"), (true, "apt install -y jq"));
        assert_eq!(split_sudo("  sudo  ls"), (true, "ls"));
        assert_eq!(split_sudo("echo sudo"), (false, "echo sudo"));
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
            .run("echo \"[$API_KEY]\"", 10, &keys, &[], Route::Proxy, &mut cancel)
            .await
            .unwrap();
        assert_eq!(out.text, "[[secret API_KEY]]");
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
            .run("echo hi && pwd", 10, &[], &[], Route::Proxy, &mut cancel)
            .await
            .unwrap();
        assert!(
            out.for_model().starts_with("exit code 0\nhi"),
            "{}",
            out.for_model()
        );
        let out = shell
            .run("exit 3", 10, &[], &[], Route::Proxy, &mut cancel)
            .await
            .unwrap();
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
        let out = shell
            .run("sleep 30", 60, &[], &[], Route::Proxy, &mut cancel)
            .await
            .unwrap();
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

    /// Feeds `bytes` in pipe-sized chunks, as `capture` does.
    fn captured(bytes: &[u8]) -> Captured {
        let mut output = Captured::default();
        bytes.chunks(8192).for_each(|chunk| output.push(chunk));
        output
    }

    /// The kept start and end of a cut text (around the omitted marker).
    fn halves(text: &str) -> (&str, &str) {
        let (head, rest) = text.split_once("\n[").expect("an omitted marker");
        (head, rest.split_once("]\n").expect("the marker's end").1)
    }

    #[test]
    fn output_keeps_a_bounded_head_and_tail() {
        // Within the margins: one buffer, cut to size at the end.
        let output = captured(
            &[
                vec![b'a'; OUTPUT_HEAD],
                vec![b'b'; OUTPUT_TAIL + 101],
                b"END".to_vec(),
            ]
            .concat(),
        );
        assert_eq!(output.omitted, 0);
        let text = output.text(&[]);
        let (head, tail) = halves(&text);
        assert_eq!((head.len(), tail.len()), (OUTPUT_HEAD, OUTPUT_TAIL));
        assert!(head.starts_with('a') && tail.ends_with("END"));
        assert!(text.contains("[104 output bytes omitted]"));
        // Far past them: the buffers stay bounded while the command runs.
        let output = captured(&[vec![b'a'; 100_000], b"END".to_vec()].concat());
        assert_eq!(output.head.len(), OUTPUT_HEAD + MARGIN);
        assert_eq!(output.tail.len(), OUTPUT_TAIL + MARGIN);
        let text = output.text(&[]);
        let (head, tail) = halves(&text);
        assert_eq!((head.len(), tail.len()), (OUTPUT_HEAD, OUTPUT_TAIL));
        assert!(tail.ends_with("END"));
        let omitted = 100_003 - OUTPUT_HEAD - OUTPUT_TAIL;
        assert!(text.contains(&format!("[{omitted} output bytes omitted]")));
        // Short output is whole.
        assert_eq!(captured(b"hello").text(&[]), "hello");
    }

    #[test]
    fn no_part_of_a_saved_value_is_left_at_a_cut() {
        // Letters the filler and the markers never use.
        let short = "QZQQZZQZZQ_QQZQ".to_string();
        let longest = "Q".repeat(crate::secrets::MAX_LEN);
        for value in [short, longest] {
            let values = [("K".to_string(), value.clone())];
            let length = value.len();
            // (total output, where the value starts): straddling each cut and
            // each captured buffer's edge, with and without bytes dropped.
            let small = OUTPUT_HEAD + OUTPUT_TAIL + MARGIN;
            let large = 200_000;
            let cases = [
                (small, OUTPUT_HEAD - 5),
                (small, small - OUTPUT_TAIL - 5),
                (large, OUTPUT_HEAD - 5),
                (large, large - OUTPUT_TAIL - 5),
                (large, OUTPUT_HEAD + MARGIN - 5),
                (large, large - OUTPUT_TAIL - MARGIN - 5),
                (large, OUTPUT_HEAD - length),
                (large, large - OUTPUT_TAIL),
            ];
            for (total, at) in cases {
                let mut bytes = vec![b'a'; total];
                bytes[at..at + length].copy_from_slice(value.as_bytes());
                let text = captured(&bytes).text(&values);
                assert!(
                    !text.contains('Q') && !text.contains('Z'),
                    "{length}-byte value at {at} of {total}: a fragment is left"
                );
                assert!(text.contains("output bytes omitted"), "{total}");
            }
            // Wholly inside what's kept, it's redacted where it was.
            let mut bytes = vec![b'a'; 200_000];
            bytes[100..100 + length].copy_from_slice(value.as_bytes());
            let tail_at = bytes.len() - length;
            bytes[tail_at..].copy_from_slice(value.as_bytes());
            let text = captured(&bytes).text(&values);
            let (head, tail) = halves(&text);
            assert!(head.starts_with(&format!("{}[secret K]a", "a".repeat(100))));
            assert!(tail.ends_with("[secret K]"));
            assert!(!text.contains('Q'));
        }
    }

    #[tokio::test]
    async fn long_output_with_a_key_keeps_its_start_end_and_hints() {
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
        let saved = [("OTHER_KEY".to_string(), "zyx987654321".to_string())];
        let (_stop, mut cancel) = watch::channel(false);
        let command = "echo \"start $API_KEY zyx987654321\"; head -c 100000 /dev/zero | tr '\\0' x; echo; \
                       echo \"$API_KEY\"; echo 'curl: (56) CONNECT tunnel failed, response 403'";
        let out = shell
            .run(command, 10, &keys, &saved, Route::Proxy, &mut cancel)
            .await
            .unwrap();
        assert!(
            out.text.starts_with("start [secret API_KEY] [secret OTHER_KEY]"),
            "{}",
            out.text.chars().take(80).collect::<String>()
        );
        assert!(out.text.contains("output bytes omitted"));
        assert!(!out.text.contains("abc123456789") && !out.text.contains("zyx987654321"));
        assert!(crate::access::refused_by_proxy(&out.text));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
