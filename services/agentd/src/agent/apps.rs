//! The native app tools (docs/desktop-apps.md §4): reads go straight to the
//! desktop broker; a click is an `app` card first, bound to the element as
//! the broker read it, and the broker checks that binding again, under its
//! lock, right before the click.

use anyhow::Result;
use base64::Engine;
use serde_json::{Value, json};

use super::{Agent, Answer, Decision, clip_summary};
use crate::desktop::{Query, Request, Response, TextPage, refusal};
use crate::guard;

/// What the model gets of a long tree, by default and at most.
const DEFAULT_CHARS: usize = 12_000;
const MAX_CHARS: usize = 100_000;

const CHANGED: &str = "not done: the app changed after the user approved (or while they decided), so nothing \
was clicked. Observe the window again, and ask again if it's still needed.";
const DENIED: &str = "denied: the user did not approve this click. Don't try another way.";
const SUMMARY_REFUSAL: &str = "not run: this needs the user's OK, and a control's name doesn't tell them what \
it does. Call it again, the same way, with a summary: what it does and why, in a few plain words for someone \
who can't see the app, like \"Save the shopping list in Mousepad\".";

impl Agent {
    pub(super) async fn app_tool(
        &self,
        task_id: &str,
        name: &str,
        args: &Value,
        schedule: Option<i64>,
        resumed: Option<Answer>,
    ) -> Result<String> {
        let desktop = self
            .desktop
            .as_ref()
            .expect("app tools exist only with the broker");
        let text = |key: &str| args[key].as_str().map(str::trim).filter(|t| !t.is_empty());
        let task = task_id.to_string();
        let max_chars = args["max_chars"]
            .as_u64()
            .map_or(DEFAULT_CHARS, |n| (n as usize).clamp(500, MAX_CHARS));
        let window = text("window").map(String::from).unwrap_or_default();
        if window.is_empty() && matches!(name, "app_observe" | "app_find" | "app_click") {
            return Ok("not done: window is required (a wN from app_windows)".into());
        }
        match name {
            "app_windows" => Ok(answer(
                desktop.request(&Request::Windows { task }).await,
                max_chars,
            )),
            "app_observe" => {
                let read_text = match &args["read_text"] {
                    Value::Object(page) => Some(TextPage {
                        field: page
                            .get("field")
                            .and_then(Value::as_str)
                            .unwrap_or("value")
                            .to_string(),
                        start: page.get("start").and_then(Value::as_u64),
                        limit: page.get("limit").and_then(Value::as_u64),
                    }),
                    _ => None,
                };
                let request = Request::Observe {
                    task,
                    window,
                    root: text("root").map(String::from),
                    read_text,
                };
                Ok(answer(desktop.request(&request).await, max_chars))
            }
            "app_find" => {
                let filter = |key: &str| text(key).map(String::from);
                let query = Query {
                    role: filter("role"),
                    name_equals: filter("name_equals"),
                    name_contains: filter("name_contains"),
                    text_equals: filter("text_equals"),
                    text_contains: filter("text_contains"),
                    state: filter("state"),
                };
                if query.is_empty() {
                    return Ok(
                        "not done: give at least one of role, name_equals, name_contains, text_equals, \
                               text_contains or state"
                            .into(),
                    );
                }
                Ok(answer(
                    desktop.request(&Request::Find { task, window, query }).await,
                    max_chars,
                ))
            }
            "app_screenshot" => {
                let response = desktop.request(&Request::Screenshot { task }).await;
                let Some(image) = response.image.as_ref().filter(|_| response.ok) else {
                    return Ok(answer(response, max_chars));
                };
                let bytes = base64::engine::general_purpose::STANDARD.decode(&image.data)?;
                let id = self.save_image(&bytes)?;
                self.emit(
                    "image",
                    json!({"task": task_id, "id": id, "url": format!("/api/v1/images/{id}"), "caption": text("caption")}),
                );
                Ok("shown to the user".into())
            }
            "app_click" => {
                if !self.settings().allow_actions {
                    return Ok(
                        "blocked: inputs on the desktop are disabled (Browser actions). Ask the user to Take over.".into(),
                    );
                }
                if schedule.is_some() {
                    return Ok("not done: app clicks require approval and a scheduled run can't ask.".into());
                }
                let Some(element) = text("ref").map(String::from) else {
                    return Ok("not done: ref is required (an rN from your latest app result)".into());
                };
                // A denial needs nothing from the app.
                if resumed.as_ref().is_some_and(|answer| !answer.approved) {
                    return Ok(DENIED.into());
                }
                let described = desktop
                    .request(&Request::Describe {
                        task: task.clone(),
                        window: window.clone(),
                        element: element.clone(),
                    })
                    .await;
                let Some(binding) = described.binding.clone().filter(|_| described.ok) else {
                    return Ok(answer(described, max_chars));
                };
                let summary = text("summary");
                if resumed.is_none() && !guard::says_why(summary, &binding.name) {
                    return Ok(SUMMARY_REFUSAL.into());
                }
                let mut card = json!({
                    "kind": "app", "action": "click", "app": binding.app, "window": binding.title,
                    "target": binding.name, "role": binding.role, "around": binding.path,
                    "summary": summary.and_then(clip_summary),
                    "reason": "may change or send something in the app",
                    "binding": binding,
                });
                if resumed.is_none() {
                    // What the user decides on: the desktop as it is now.
                    let shot = desktop.request(&Request::Screenshot { task: task.clone() }).await;
                    if let Some(image) = shot.image.filter(|_| shot.ok)
                        && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&image.data)
                    {
                        match self.save_image(&bytes) {
                            Ok(id) => card["image"] = json!(format!("/api/v1/images/{id}")),
                            Err(err) => tracing::warn!(%err, "couldn't save a card's picture"),
                        }
                    }
                }
                match self.approve(task_id, card, resumed, false)? {
                    Decision::Approved => {}
                    Decision::Changed => return Ok(CHANGED.into()),
                    Decision::Denied | Decision::Unattended => {
                        return Ok(
                            "denied: the user did not approve this click. Don't try another way.".into(),
                        );
                    }
                }
                // The setting may have changed while the card waited.
                if !self.settings().allow_actions {
                    return Ok("blocked: inputs on the desktop were disabled.".into());
                }
                self.acted(task_id)?;
                let clicked = desktop
                    .request(&Request::Click {
                        task,
                        window,
                        element,
                        expect: binding,
                    })
                    .await;
                if clicked.error.as_deref() == Some(refusal::CHANGED) {
                    return Ok(CHANGED.into());
                }
                Ok(answer(clicked, max_chars))
            }
            _ => Ok(format!("not done: there's no tool {name}")),
        }
    }
}

/// The broker's answer, for the model: HCU's text (its evidence line first),
/// or what the broker refused and what to do instead.
fn answer(response: Response, max_chars: usize) -> String {
    if !response.ok {
        let message = response.message.unwrap_or_default();
        return match response.error.as_deref() {
            Some(refusal::UNAVAILABLE) => format!("error: {message}"),
            Some(refusal::INSPECT_FIRST) => format!("not done: {message}"),
            Some(refusal::STALE) => format!("not done (stale ref): {message}"),
            Some(refusal::NOT_ALLOWED) => {
                format!("not done: {message}. Use app_windows for the ones you may use.")
            }
            _ => format!("not done: {message}"),
        };
    }
    let text = response.text.unwrap_or_default();
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!(
            "{}\n[… cut at {max_chars} characters: use app_find, or a root, to read less]",
            &text[..cut]
        ),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Park, test_agent};
    use crate::browser::Browser;
    use crate::desktop::{Binding, Image};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::watch;

    /// A broker that answers from a script and keeps what it was asked.
    struct FakeBroker {
        asked: Arc<Mutex<Vec<Request>>>,
        /// What Describe finds now.
        element: Arc<Mutex<Binding>>,
        socket: std::path::PathBuf,
    }

    impl Drop for FakeBroker {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    fn save_binding() -> Binding {
        Binding {
            epoch: "e1-1".into(),
            window: "w1".into(),
            element: "r13".into(),
            app: "Mousepad".into(),
            app_id: "org.xfce.mousepad".into(),
            pid: 41,
            exe: "/usr/bin/mousepad".into(),
            title: "*notes.txt - Mousepad".into(),
            role: "menuitem".into(),
            name: "Save".into(),
            path: "menu “File” › menuitem “Save” — Save the current document".into(),
        }
    }

    fn fake_broker(socket: &std::path::Path) -> FakeBroker {
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let element = Arc::new(Mutex::new(save_binding()));
        let (log, now) = (asked.clone(), element.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                BufReader::new(read).read_line(&mut line).await.unwrap();
                let request: Request = serde_json::from_str(&line).unwrap();
                log.lock().unwrap().push(request.clone());
                let response = match request {
                    Request::Describe { .. } => Response {
                        ok: true,
                        binding: Some(now.lock().unwrap().clone()),
                        ..Response::default()
                    },
                    Request::Screenshot { .. } => Response {
                        ok: true,
                        image: Some(Image {
                            mime: "image/png".into(),
                            data: "iVBORw0KGgo=".into(),
                        }),
                        ..Response::default()
                    },
                    Request::Click { expect, .. } if expect != *now.lock().unwrap() => {
                        Response::refused(refusal::CHANGED, "r13 isn't what was approved any more")
                    }
                    _ => Response {
                        ok: true,
                        text: Some("{\"ok\":true,\"input_emitted\":true,\"completed\":2,\"requested\":2}\nwindows: no change".into()),
                        ..Response::default()
                    },
                };
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                let _ = write.write_all(&bytes).await;
            }
        });
        FakeBroker {
            asked,
            element,
            socket: socket.to_path_buf(),
        }
    }

    fn with_apps() -> (Arc<Agent>, std::path::PathBuf, FakeBroker) {
        let (agent, dir) = test_agent();
        // Short: a socket's path has to fit in about a hundred bytes.
        let socket = std::env::temp_dir().join(format!(
            "nb-{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        let broker = fake_broker(&socket);
        let mut cfg = agent.cfg.clone();
        cfg.desktop_apps = true;
        cfg.desktop_broker = socket;
        (Arc::new(Agent::new(cfg).unwrap()), dir, broker)
    }

    fn clicks(broker: &FakeBroker) -> Vec<Request> {
        broker
            .asked
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, Request::Click { .. }))
            .cloned()
            .collect()
    }

    fn save() -> Value {
        json!({"window": "w1", "ref": "r13", "summary": "Save the shopping list"})
    }

    async fn park(agent: &Agent) -> Park {
        agent
            .app_tool("t1", "app_click", &save(), None, None)
            .await
            .unwrap_err()
            .downcast::<Park>()
            .expect("a click asks first")
    }

    fn answered(park: &Park, approved: bool) -> Option<Answer> {
        Some(Answer {
            approved,
            card: park.card.clone(),
            by: None,
            edited: None,
        })
    }

    #[tokio::test]
    async fn a_click_is_a_card_first_and_runs_bound_to_what_it_showed() {
        let (agent, dir, broker) = with_apps();
        let park = park(&agent).await;
        let card = &park.card;
        assert_eq!(card["kind"], "app");
        assert_eq!(card["action"], "click");
        assert_eq!(card["app"], "Mousepad");
        assert_eq!(card["window"], "*notes.txt - Mousepad");
        assert_eq!(
            (card["target"].as_str(), card["role"].as_str()),
            (Some("Save"), Some("menuitem"))
        );
        assert_eq!(card["around"], save_binding().path);
        assert_eq!(card["summary"], "Save the shopping list");
        assert!(card["image"].as_str().unwrap().starts_with("/api/v1/images/"));
        assert_eq!(card["binding"]["pid"], 41);
        assert!(clicks(&broker).is_empty(), "nothing clicked before the OK");
        let done = agent
            .app_tool("t1", "app_click", &save(), None, answered(&park, true))
            .await
            .unwrap();
        assert!(done.starts_with("{\"ok\":true"), "{done}");
        let sent = clicks(&broker);
        assert_eq!(sent.len(), 1);
        let Request::Click { expect, element, .. } = &sent[0] else {
            unreachable!()
        };
        assert_eq!((expect, element.as_str()), (&save_binding(), "r13"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn denied_off_scheduled_or_changed_sends_no_input() {
        let (agent, dir, broker) = with_apps();
        // Denied.
        let park = park(&agent).await;
        let denied = agent
            .app_tool("t1", "app_click", &save(), None, answered(&park, false))
            .await
            .unwrap();
        assert!(denied.starts_with("denied:"), "{denied}");
        let describes = |broker: &FakeBroker| {
            broker
                .asked
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, Request::Describe { .. }))
                .count()
        };
        assert_eq!(describes(&broker), 1, "a denial asks the app nothing more");
        // Approved, but the element changed while the user decided (the
        // window's title lost its "*": someone saved).
        let park = super::tests::park(&agent).await;
        broker.element.lock().unwrap().title = "notes.txt - Mousepad".into();
        let changed = agent
            .app_tool("t1", "app_click", &save(), None, answered(&park, true))
            .await
            .unwrap();
        assert!(changed.starts_with("not done: the app changed"), "{changed}");
        // A scheduled run can't ask; with Browser actions off nobody is asked.
        let scheduled = agent
            .app_tool("t1", "app_click", &save(), Some(3), None)
            .await
            .unwrap();
        assert!(scheduled.contains("can't ask"), "{scheduled}");
        agent.update_settings(None, Some(false)).unwrap();
        let off = agent
            .app_tool("t1", "app_click", &save(), None, None)
            .await
            .unwrap();
        assert!(off.starts_with("blocked:"), "{off}");
        assert!(clicks(&broker).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn reads_go_through_and_the_tools_exist_only_with_the_broker() {
        let (agent, dir, broker) = with_apps();
        let mut browser = Browser::new(agent.cdp.clone());
        let (_stop, mut cancel) = watch::channel(false);
        let found = agent
            .call_tool(
                &mut browser,
                "t1",
                "app_find",
                &json!({"window": "w1", "role": "menu item", "name_contains": "Save"}),
                false,
                None,
                None,
                &mut cancel,
            )
            .await
            .unwrap();
        assert!(found.starts_with("{\"ok\":true"), "{found}");
        let Request::Find { query, .. } = broker.asked.lock().unwrap()[0].clone() else {
            panic!()
        };
        assert_eq!(
            (query.role.as_deref(), query.name_contains.as_deref()),
            (Some("menu item"), Some("Save"))
        );
        let missing = agent
            .app_tool("t1", "app_find", &json!({"window": "w1"}), None, None)
            .await
            .unwrap();
        assert!(missing.contains("at least one of"), "{missing}");
        // Without the broker configured, app_* are no tools at all.
        let (plain, plain_dir) = test_agent();
        let unknown = plain
            .call_tool(
                &mut browser,
                "t1",
                "app_windows",
                &json!({}),
                false,
                None,
                None,
                &mut cancel,
            )
            .await;
        assert!(!format!("{unknown:?}").contains("ok\\\":true"), "{unknown:?}");
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(plain_dir).unwrap();
    }

    #[test]
    fn refusals_say_what_to_do_and_long_trees_are_cut() {
        let refused = Response::refused(
            refusal::STALE,
            "r9 isn't from your latest results; observe or find it again",
        );
        assert!(answer(refused, 100).starts_with("not done (stale ref): r9"));
        let down = Response::refused(refusal::UNAVAILABLE, "the desktop's app broker isn't answering");
        assert!(answer(down, 100).starts_with("error: "));
        let tree = Response {
            ok: true,
            text: Some("{\"ok\":true}\n".to_string() + &"x".repeat(900)),
            ..Response::default()
        };
        let cut = answer(tree, 500);
        assert!(cut.starts_with("{\"ok\":true}") && cut.contains("cut at 500 characters"));
    }
}
