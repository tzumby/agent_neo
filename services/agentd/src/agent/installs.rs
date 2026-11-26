//! install_app (docs/desktop-apps.md §10): the agent asks for a desktop app
//! from the Snap Store, the user approves an `install` card, and root installs
//! it through the privileged helper (`install-snap`), which checks the store
//! again and puts the app's launchers on the desktop and in the menu.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::{Agent, Answer, Decision};
use crate::snaps;

/// A snap is a few hundred MB at most; the helper allows 30 minutes too.
const INSTALL_DEADLINE: Duration = Duration::from_secs(1900);
const DENIED: &str = "denied: the user did not approve installing it. Don't try another way.";

impl Agent {
    pub(super) async fn install_app(
        &self,
        task_id: &str,
        args: &Value,
        schedule: Option<i64>,
        tainted: bool,
        resumed: Option<Answer>,
    ) -> Result<String> {
        if schedule.is_some() {
            return Ok(
                "not done: installing an app needs the user's OK, and a scheduled run can't ask.".into(),
            );
        }
        if resumed.as_ref().is_some_and(|answer| !answer.approved) {
            return Ok(DENIED.into());
        }
        let socket = self.cfg.snapd_socket.as_str();
        let asked = args["name"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let found = if snaps::valid_name(&asked) {
            snaps::find(socket, &asked)
                .await
                .context("asking the Snap Store")?
        } else {
            None
        };
        let Some(snap) = found else {
            // Not a snap's name: what the store finds for those words.
            let similar = snaps::search(socket, &asked).await.unwrap_or_default();
            let list: Vec<String> = similar
                .iter()
                .map(|s| {
                    let mark = if s.verified() { " ✓" } else { "" };
                    format!(
                        "{} ({}, by {}{mark}, {})",
                        s.name, s.title, s.publisher, s.confinement
                    )
                })
                .collect();
            return Ok(if list.is_empty() {
                format!("not done: the Snap Store has no app named {asked:?}.")
            } else {
                format!(
                    "not done: the Snap Store has no app named {asked:?}. It finds: {}. Call install_app with \
                     one of those names (only strictly confined ones from verified publishers, ✓).",
                    list.join("; ")
                )
            });
        };
        if let Some(version) = snaps::installed(socket, &snap.name).await? {
            return Ok(format!(
                "{} {version} is already installed (snap {}); it's in the Applications menu.",
                snap.title, snap.name
            ));
        }
        if let Some(why_not) = snap.refusal() {
            return Ok(format!(
                "not done: {why_not}. Only strictly confined apps from verified publishers can be installed \
                 this way; the owner can install others over SSM."
            ));
        }
        let card = json!({
            "kind": "install", "source": "Snap Store", "name": snap.name, "app": snap.title,
            "publisher": snap.publisher, "publisher_id": snap.publisher_id, "verified": snap.verified(),
            "version": snap.version, "channel": snap.channel, "confinement": snap.confinement,
            "summary": snap.summary, "size": snap.size,
            "why": args["why"].as_str().map(str::trim).filter(|w| !w.is_empty()),
            "reason": "installs an app on your desktop, as root", "tainted": tainted,
        });
        match self.approve(task_id, card, resumed, false)? {
            Decision::Approved => {}
            Decision::Changed => {
                return Ok(
                    "not done: the store's entry changed (a new version or publisher) while the user \
                           decided. Ask again if it's still needed."
                        .into(),
                );
            }
            Decision::Denied | Decision::Unattended => return Ok(DENIED.into()),
        }
        self.acted(task_id)?;
        tracing::info!(task = %task_id, snap = %snap.name, "installing an approved app");
        match run_install(&snap.name, &snap.publisher_id).await {
            Ok(done) => {
                let version = done["version"].as_str().unwrap_or(&snap.version);
                let on_desktop = done["launchers"].as_array().is_some_and(|l| !l.is_empty());
                self.emit(
                    "app.installed",
                    json!({"task": task_id, "name": snap.name, "app": snap.title, "version": version}),
                );
                Ok(format!(
                    "installed {} {version} (snap {}). {} It's in the Applications menu. Apps started from now on \
                     can be read by the app tools if they're on the list the owner keeps.",
                    snap.title,
                    snap.name,
                    if on_desktop {
                        "Its launcher is on the desktop."
                    } else {
                        "It has no launcher of its own."
                    },
                ))
            }
            Err(err) => Ok(format!(
                "error: the install didn't finish ({err:#}). Nothing else was changed."
            )),
        }
    }
}

/// The privileged helper's `install-snap`, as root through sudo: the request
/// on stdin, what it installed (JSON) on stdout.
async fn run_install(name: &str, publisher_id: &str) -> Result<Value> {
    let mut child = tokio::process::Command::new("/usr/bin/sudo")
        .args(["-n", "--", crate::shell::PRIVILEGED_HELPER, "install-snap"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("starting the install helper")?;
    let request = json!({"name": name, "publisher": publisher_id}).to_string();
    let mut stdin = child.stdin.take().context("opening the helper's input")?;
    stdin.write_all(request.as_bytes()).await?;
    drop(stdin);
    let output = tokio::time::timeout(INSTALL_DEADLINE, child.wait_with_output())
        .await
        .context("the install took too long")??;
    if !output.status.success() {
        anyhow::bail!("the install helper exited with {}", output.status);
    }
    serde_json::from_slice(&output.stdout).context("the install helper's answer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Park, test_agent};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    /// A snapd that knows slack (verified, strict) and a classic one.
    fn fake_snapd() -> String {
        let socket = std::env::temp_dir()
            .join(format!(
                "sd-{}.sock",
                &uuid::Uuid::new_v4().simple().to_string()[..10]
            ))
            .display()
            .to_string();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 2048];
                let n = stream.read(&mut request).await.unwrap();
                let line = String::from_utf8_lossy(&request[..n])
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                let snap = |name: &str, confinement: &str| {
                    json!({"name": name, "title": name, "type": "app", "version": "1.0", "channel": "stable",
                           "confinement": confinement, "summary": "", "download-size": 1000,
                           "publisher": {"id": "GKq9csPRQp1E5kUglZG9QTqDEBLrcszO", "display-name": "Pub", "validation": "verified"}})
                };
                let (status, body) = if line.starts_with("GET /v2/find?name=slack ") {
                    ("200 OK", json!({"result": [snap("slack", "strict")]}))
                } else if line.starts_with("GET /v2/find?name=code ") {
                    ("200 OK", json!({"result": [snap("code", "classic")]}))
                } else if line.starts_with("GET /v2/find?q=") {
                    ("200 OK", json!({"result": [snap("slack", "strict")]}))
                } else {
                    ("404 Not Found", json!({"result": {"message": "not found"}}))
                };
                let reply = format!("HTTP/1.0 {status}\r\nContent-Type: application/json\r\n\r\n{body}");
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });
        socket
    }

    fn with_snapd() -> (Arc<Agent>, std::path::PathBuf, String) {
        let (agent, dir) = test_agent();
        let socket = fake_snapd();
        let mut cfg = agent.cfg.clone();
        cfg.app_installs = true;
        cfg.snapd_socket = socket.clone();
        (Arc::new(Agent::new(cfg).unwrap()), dir, socket)
    }

    #[tokio::test]
    async fn an_install_is_a_card_with_the_stores_facts() {
        let (agent, dir, socket) = with_snapd();
        let args = json!({"name": "Slack", "why": "So I can use Slack as an app"});
        let park = agent
            .install_app("t1", &args, None, true, None)
            .await
            .unwrap_err()
            .downcast::<Park>()
            .expect("it asks first");
        let card = &park.card;
        assert_eq!(
            (card["kind"].as_str(), card["name"].as_str()),
            (Some("install"), Some("slack"))
        );
        assert_eq!(card["publisher_id"], "GKq9csPRQp1E5kUglZG9QTqDEBLrcszO");
        assert_eq!(
            (card["verified"].as_bool(), card["confinement"].as_str()),
            (Some(true), Some("strict"))
        );
        assert_eq!(card["tainted"], true);
        assert_eq!(card["why"], "So I can use Slack as an app");
        // Denied: nothing more is asked or run.
        let answer = Answer {
            approved: false,
            card: park.card.clone(),
            by: None,
            edited: None,
            plan: None,
            note: None,
            for_task: false,
        };
        let denied = agent
            .install_app("t1", &args, None, true, Some(answer))
            .await
            .unwrap();
        assert!(denied.starts_with("denied:"), "{denied}");
        let _ = std::fs::remove_file(&socket);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn refuses_what_root_would_and_never_in_a_scheduled_run() {
        let (agent, dir, socket) = with_snapd();
        let classic = agent
            .install_app("t1", &json!({"name": "code"}), None, false, None)
            .await
            .unwrap();
        assert!(classic.contains("isn't strictly confined"), "{classic}");
        let scheduled = agent
            .install_app("t1", &json!({"name": "slack"}), Some(2), false, None)
            .await
            .unwrap();
        assert!(scheduled.contains("scheduled run can't ask"), "{scheduled}");
        // Not a name: what the store finds instead.
        let guess = agent
            .install_app("t1", &json!({"name": "slack desktop"}), None, false, None)
            .await
            .unwrap();
        assert!(guess.contains("It finds: slack"), "{guess}");
        let _ = std::fs::remove_file(&socket);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
