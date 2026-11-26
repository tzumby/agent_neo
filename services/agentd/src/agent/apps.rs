//! The native app tools (docs/desktop-apps.md §5): reads go straight to the
//! desktop broker; input is an `app` card first, bound to every step's target
//! as the broker read it, and the broker checks those bindings again, under
//! its lock, right before it sends anything. With a model that sees images,
//! screenshots go to the model too, and pixel input (app_gui) is bound to the
//! window under each point of the screenshot it saw.

use anyhow::Result;
use base64::Engine;
use serde_json::{Value, json};

use super::{Agent, Answer, Decision, clip_summary};
use crate::desktop::steps::{GuiAction, Step};
use crate::desktop::{Binding, GuiBinding, Query, Request, Response, TextPage, refusal};
use crate::guard;

/// What the model gets of a long tree, by default and at most.
const DEFAULT_CHARS: usize = 12_000;
const MAX_CHARS: usize = 100_000;

const CHANGED: &str = "not done: the app changed after the user approved (or while they decided), so nothing \
was sent. Look again (app_observe, or app_screenshot for pixel input), and ask again if it's still needed.";
const DENIED: &str = "denied: the user did not approve this. Don't try another way.";
const SUMMARY_REFUSAL: &str = "not run: this needs the user's OK, and the steps alone don't tell them what it's \
for. Call it again, the same way, with a summary: what it does and why, in a few plain words for someone who \
can't see the app, like \"Save the shopping list in Mousepad\".";
const NO_ACTIONS: &str =
    "blocked: inputs on the desktop are disabled (Browser actions). Ask the user to Take over.";
const NO_SCHEDULED: &str = "not done: input in apps needs the user's OK, and a scheduled run can't ask.";

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
        if window.is_empty() && matches!(name, "app_observe" | "app_find" | "app_act") {
            return Ok("not done: window is required (a wN from app_windows)".into());
        }
        let vision = self.sees_images(task_id);
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
                let id = self.save_image(&base64::engine::general_purpose::STANDARD.decode(&image.data)?)?;
                // The user sees it when there's no model to see it, or when asked to.
                if !vision || args["show"] == true {
                    self.emit(
                        "image",
                        json!({"task": task_id, "id": id, "url": format!("/api/v1/images/{id}"), "caption": text("caption")}),
                    );
                }
                if !vision {
                    return Ok("shown to the user".into());
                }
                let (w, h) = response.frame.unwrap_or_default();
                self.give_model_image(task_id, &id, Some((w, h)));
                Ok(format!(
                    "screenshot taken ({w}×{h} pixels, the whole desktop): it's attached after these results. \
                     app_gui's x/y are its pixels."
                ))
            }
            "app_zoom" if vision => {
                let region: Option<[f64; 4]> = serde_json::from_value(args["region"].clone()).ok();
                let Some(region) = region else {
                    return Ok(
                        "not done: region is [x0, y0, x1, y1] in the latest screenshot's pixels".into(),
                    );
                };
                let response = desktop.request(&Request::Zoom { task, region }).await;
                let Some(image) = response.image.clone().filter(|_| response.ok) else {
                    return Ok(answer(response, max_chars));
                };
                let id = self.save_image(&base64::engine::general_purpose::STANDARD.decode(&image.data)?)?;
                self.give_model_image(task_id, &id, None);
                let note = response.text.unwrap_or_default();
                Ok(format!(
                    "{note}\nThe enlarged region is attached after these results."
                ))
            }
            "app_act" => {
                if !self.settings().allow_actions {
                    return Ok(NO_ACTIONS.into());
                }
                if schedule.is_some() {
                    return Ok(NO_SCHEDULED.into());
                }
                let steps: Vec<Step> = match serde_json::from_value(args["steps"].clone()) {
                    Ok(steps) => steps,
                    Err(err) => return Ok(format!("not done: steps don't parse ({err}). {STEP_FORMS}")),
                };
                if steps
                    .iter()
                    .filter_map(Step::text)
                    .any(|t| self.secrets.holds_a_value(t))
                {
                    return Ok(
                        "not done: the text includes a saved key's value, which the agent never types. \
                               Ask the user to Take over if it must go in."
                            .into(),
                    );
                }
                // A denial needs nothing from the app.
                if resumed.as_ref().is_some_and(|answer| !answer.approved) {
                    return Ok(DENIED.into());
                }
                let prepared = desktop
                    .request(&Request::Prepare {
                        task: task.clone(),
                        window: window.clone(),
                        steps: steps.clone(),
                    })
                    .await;
                let Some(bindings) = prepared.bindings.clone().filter(|_| prepared.ok) else {
                    return Ok(answer(prepared, max_chars));
                };
                let summary = text("summary");
                let first_name = bindings.first().map_or("", |b| b.name.as_str());
                if resumed.is_none() && !guard::says_why(summary, first_name) {
                    return Ok(SUMMARY_REFUSAL.into());
                }
                let mut card = act_card(&window, &steps, &bindings);
                card["summary"] = json!(summary.and_then(clip_summary));
                // A picture for the card, unless the plan covers it (no card).
                if resumed.is_none()
                    && self.covering(task_id, &card).is_none()
                    && !self.plan_first(task_id, &card)
                {
                    self.picture_for(&mut card, desktop, &task).await;
                }
                match self.approve(task_id, card, resumed, false)? {
                    Decision::Approved => {}
                    Decision::Changed => return Ok(CHANGED.into()),
                    Decision::Denied | Decision::Unattended => return Ok(DENIED.into()),
                }
                // The setting may have changed while the card waited.
                if !self.settings().allow_actions {
                    return Ok("blocked: inputs on the desktop were disabled.".into());
                }
                self.acted(task_id)?;
                let done = desktop
                    .request(&Request::Act {
                        task,
                        window,
                        steps,
                        expect: bindings,
                    })
                    .await;
                if done.error.as_deref() == Some(refusal::CHANGED) {
                    return Ok(CHANGED.into());
                }
                Ok(answer(done, max_chars))
            }
            "app_gui" if vision => {
                if !self.settings().allow_actions {
                    return Ok(NO_ACTIONS.into());
                }
                if schedule.is_some() {
                    return Ok(NO_SCHEDULED.into());
                }
                let actions: Vec<GuiAction> = match serde_json::from_value(args["actions"].clone()) {
                    Ok(actions) => actions,
                    Err(err) => return Ok(format!("not done: actions don't parse ({err}). {GUI_FORMS}")),
                };
                if actions
                    .iter()
                    .filter_map(GuiAction::text)
                    .any(|t| self.secrets.holds_a_value(t))
                {
                    return Ok(
                        "not done: the text includes a saved key's value, which the agent never types."
                            .into(),
                    );
                }
                if resumed.as_ref().is_some_and(|answer| !answer.approved) {
                    return Ok(DENIED.into());
                }
                let prepared = desktop
                    .request(&Request::PrepareGui {
                        task: task.clone(),
                        actions: actions.clone(),
                    })
                    .await;
                let Some(binding) = prepared.gui.clone().filter(|_| prepared.ok) else {
                    return Ok(answer(prepared, max_chars));
                };
                let summary = text("summary");
                if resumed.is_none() && !guard::says_why(summary, "") {
                    return Ok(SUMMARY_REFUSAL.into());
                }
                let mut card = gui_card(&actions, &binding);
                card["summary"] = json!(summary.and_then(clip_summary));
                // The screenshot the model planned on: the card marks its points on it.
                if let Some(frame) = self.model_frames.lock().unwrap().get(task_id) {
                    card["image"] = json!(format!("/api/v1/images/{frame}"));
                }
                match self.approve(task_id, card, resumed, false)? {
                    Decision::Approved => {}
                    Decision::Changed => return Ok(CHANGED.into()),
                    Decision::Denied | Decision::Unattended => return Ok(DENIED.into()),
                }
                if !self.settings().allow_actions {
                    return Ok("blocked: inputs on the desktop were disabled.".into());
                }
                self.acted(task_id)?;
                let done = desktop
                    .request(&Request::Gui {
                        task,
                        actions,
                        expect: binding,
                    })
                    .await;
                if done.error.as_deref() == Some(refusal::CHANGED) {
                    return Ok(CHANGED.into());
                }
                let attached = match done.image.clone().filter(|_| done.ok) {
                    Some(image) => {
                        let id =
                            self.save_image(&base64::engine::general_purpose::STANDARD.decode(&image.data)?)?;
                        self.give_model_image(task_id, &id, done.frame);
                        "\nThe desktop after it is attached after these results (the new frame for app_gui)."
                    }
                    None => "",
                };
                Ok(format!("{}{attached}", answer(done, max_chars)))
            }
            _ => Ok(format!("not done: there's no tool {name}")),
        }
    }

    /// The card's picture: the desktop as it is now.
    async fn picture_for(&self, card: &mut Value, desktop: &crate::desktop::client::Desktop, task: &str) {
        let shot = desktop
            .request(&Request::Screenshot {
                task: task.to_string(),
            })
            .await;
        if let Some(image) = shot.image.filter(|_| shot.ok)
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&image.data)
        {
            match self.save_image(&bytes) {
                Ok(id) => card["image"] = json!(format!("/api/v1/images/{id}")),
                Err(err) => tracing::warn!(%err, "couldn't save a card's picture"),
            }
        }
    }

    /// Queues a saved image for the task's model (attached before its next
    /// turn); a screenshot also becomes the frame its pixel cards show.
    fn give_model_image(&self, task_id: &str, id: &str, frame: Option<(u32, u32)>) {
        self.model_images
            .lock()
            .unwrap()
            .entry(task_id.to_string())
            .or_default()
            .push(id.to_string());
        if frame.is_some() {
            self.model_frames
                .lock()
                .unwrap()
                .insert(task_id.to_string(), id.to_string());
        }
    }
}

/// How the steps look, for a model that got them wrong.
const STEP_FORMS: &str = "Each step is one of: {\"type\":\"click\",\"ref\":\"r12\"}, {\"type\":\"type\",\"ref\":\"r7\",\"text\":\"…\"}, \
{\"type\":\"keypress\",\"keys\":[\"CTRL\",\"S\"]}, {\"type\":\"set_value\",\"ref\":\"r9\",\"value\":\"…\"}, \
{\"type\":\"select\",\"ref\":\"r7\",\"text\":\"…\"}, {\"type\":\"scroll\",\"ref\":\"r3\",\"direction\":\"down\",\"pages\":1}, \
{\"type\":\"activate\",\"window\":\"w2\"}, {\"type\":\"close\",\"window\":\"w2\"}, \
{\"type\":\"wait\",\"window\":\"Save As\"} (or \"settle\":true, \"query\":{…}, \"ref\"+\"state\", \"text\"; \"timeout_ms\").";
const GUI_FORMS: &str = "Each action is one of: {\"type\":\"click\",\"x\":10,\"y\":20} (\"button\":\"right\"), \
{\"type\":\"double_click\",\"x\":…,\"y\":…}, {\"type\":\"move\",\"x\":…,\"y\":…}, \
{\"type\":\"drag\",\"path\":[{\"x\":…,\"y\":…},{\"x\":…,\"y\":…}]}, {\"type\":\"scroll\",\"x\":…,\"y\":…,\"scroll_y\":3}, \
{\"type\":\"type\",\"text\":\"…\"}, {\"type\":\"keypress\",\"keys\":[\"ENTER\"]}, {\"type\":\"wait\"}.";

/// An accessibility batch's card: each step as the app has it (from its
/// binding), the full text of anything typed or set, and the bindings.
fn act_card(window: &str, steps: &[Step], bindings: &[Binding]) -> Value {
    let described: Vec<Value> = steps
        .iter()
        .zip(bindings)
        .map(|(step, b)| describe_step(step, b))
        .collect();
    let inputs: Vec<&Value> = steps
        .iter()
        .zip(&described)
        .filter(|(step, _)| step.is_input())
        .map(|(_, d)| d)
        .collect();
    let first = bindings.first().cloned().unwrap_or_default();
    let mut card = json!({
        "kind": "app", "app": first.app, "window": first.title, "steps": described,
        "reason": "may change or send something in the app",
        "binding": {"window": window, "steps": steps, "targets": bindings},
    });
    // One input: its fields on the card itself, as a single click always had.
    if let [only] = inputs.as_slice() {
        for key in ["action", "target", "role", "around", "text", "key"] {
            if !only[key].is_null() {
                card[key] = only[key].clone();
            }
        }
    } else {
        card["action"] = json!("act");
    }
    card
}

fn describe_step(step: &Step, b: &Binding) -> Value {
    let element =
        |action: &str| json!({"action": action, "target": b.name, "role": b.role, "around": b.path});
    match step {
        Step::Click { pointer, .. } => {
            let mut d = element("click");
            if *pointer {
                d["pointer"] = json!(true);
            }
            d
        }
        Step::Type { text, .. } => {
            let mut d = element("type");
            d["text"] = json!(text);
            d
        }
        Step::SetValue { value, .. } => {
            let mut d = element("set_value");
            d["text"] = json!(value);
            d
        }
        Step::Select { text, to_ref, .. } => {
            let mut d = element("select");
            if let Some(text) = text {
                d["text"] = json!(text);
            }
            if to_ref.is_some() {
                d["range"] = json!(true);
            }
            d
        }
        Step::Scroll { direction, pages, .. } => {
            let mut d = element("scroll");
            d["direction"] = json!(direction);
            d["pages"] = json!(pages);
            d
        }
        Step::Keypress { keys } => json!({"action": "key", "key": keys.join("+"), "window": b.title}),
        Step::Activate { .. } => json!({"action": "activate", "window": b.title}),
        Step::Close { .. } => json!({"action": "close", "window": b.title}),
        Step::Wait(wait) => {
            let what = wait
                .window
                .as_ref()
                .map(|w| format!("a window “{w}”"))
                .or_else(|| wait.text.as_ref().map(|t| format!("“{t}”")))
                .or_else(|| wait.state.as_ref().map(|s| format!("it to be {s}")))
                .unwrap_or_else(|| {
                    if wait.settle {
                        "the app to settle".into()
                    } else {
                        "the app".into()
                    }
                });
            json!({"action": "wait", "for": what, "gone": wait.gone})
        }
    }
}

/// A pixel batch's card: each action with where it lands (the window under
/// its point, in the screenshot the model saw), and the frame's size, so the
/// apps can mark the points on the picture.
fn gui_card(actions: &[GuiAction], binding: &GuiBinding) -> Value {
    let described: Vec<Value> = actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            let target = binding.targets.iter().find(|t| t.action == i);
            let mut d = serde_json::to_value(action).unwrap_or_default();
            if let Some(fields) = d.as_object_mut()
                && let Some(kind) = fields.remove("type")
            {
                fields.insert("action".into(), kind);
            }
            if let Some(keys) = d.get("keys").and_then(Value::as_array).filter(|k| !k.is_empty()) {
                d["key"] = json!(
                    keys.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("+")
                );
            }
            if let Some(target) = target {
                d["window"] = json!(target.title);
                d["app"] = json!(target.app);
            }
            d
        })
        .collect();
    let first = binding.targets.first().cloned().unwrap_or_default();
    json!({
        "kind": "app", "action": "gui", "app": first.app, "window": first.title, "steps": described,
        "frame": {"width": binding.size.0, "height": binding.size.1},
        "reason": "may change or send something in the app",
        "binding": {"actions": actions, "gui": binding},
    })
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
    use crate::desktop::{GuiTarget, Image};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::watch;

    /// A broker that answers from a script and keeps what it was asked.
    struct FakeBroker {
        asked: Arc<Mutex<Vec<Request>>>,
        /// What Prepare finds now, for every element step.
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

    fn gui_binding() -> GuiBinding {
        GuiBinding {
            epoch: "e1-1".into(),
            frame: 3,
            size: (1229, 768),
            targets: vec![GuiTarget {
                action: 0,
                at: Some((100.0, 120.0)),
                app: "Mousepad".into(),
                title: "notes.txt - Mousepad".into(),
                pid: 41,
                exe: "/usr/bin/mousepad".into(),
            }],
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
                let picture = || Image {
                    mime: "image/png".into(),
                    data: "iVBORw0KGgo=".into(),
                };
                let done = || {
                    Response {
                    ok: true,
                    text: Some("{\"ok\":true,\"input_emitted\":true,\"completed\":1,\"requested\":1}\nwindows: no change".into()),
                    ..Response::default()
                }
                };
                let response = match request {
                    Request::Prepare { steps, .. } => Response {
                        ok: true,
                        bindings: Some(steps.iter().map(|_| now.lock().unwrap().clone()).collect()),
                        ..Response::default()
                    },
                    Request::Screenshot { .. } => Response {
                        ok: true,
                        image: Some(picture()),
                        frame: Some((1229, 768)),
                        ..Response::default()
                    },
                    Request::Act { expect, .. } if expect.iter().any(|b| *b != *now.lock().unwrap()) => {
                        Response::refused(refusal::CHANGED, "not what was approved any more")
                    }
                    Request::PrepareGui { .. } => Response {
                        ok: true,
                        gui: Some(gui_binding()),
                        ..Response::default()
                    },
                    Request::Gui { .. } => Response {
                        image: Some(picture()),
                        frame: Some((1229, 768)),
                        ..done()
                    },
                    _ => done(),
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

    fn sent(broker: &FakeBroker, op: fn(&Request) -> bool) -> Vec<Request> {
        broker
            .asked
            .lock()
            .unwrap()
            .iter()
            .filter(|r| op(r))
            .cloned()
            .collect()
    }

    fn acts(broker: &FakeBroker) -> Vec<Request> {
        sent(broker, |r| matches!(r, Request::Act { .. } | Request::Gui { .. }))
    }

    fn save() -> Value {
        json!({"window": "w1", "summary": "Save the shopping list", "steps": [{"type": "click", "ref": "r13"}]})
    }

    async fn park(agent: &Agent, args: &Value) -> Park {
        agent
            .app_tool("t1", "app_act", args, None, None)
            .await
            .unwrap_err()
            .downcast::<Park>()
            .expect("input asks first")
    }

    fn answered(park: &Park, approved: bool) -> Option<Answer> {
        Some(Answer {
            approved,
            card: park.card.clone(),
            by: None,
            edited: None,
            plan: None,
            note: None,
            for_task: false,
        })
    }

    #[tokio::test]
    async fn a_click_is_a_card_first_and_runs_bound_to_what_it_showed() {
        let (agent, dir, broker) = with_apps();
        let park = park(&agent, &save()).await;
        let card = &park.card;
        assert_eq!(
            (card["kind"].as_str(), card["action"].as_str()),
            (Some("app"), Some("click"))
        );
        assert_eq!(card["app"], "Mousepad");
        assert_eq!(card["window"], "*notes.txt - Mousepad");
        assert_eq!(
            (card["target"].as_str(), card["role"].as_str()),
            (Some("Save"), Some("menuitem"))
        );
        assert_eq!(card["around"], save_binding().path);
        assert_eq!(card["summary"], "Save the shopping list");
        assert_eq!(card["steps"].as_array().unwrap().len(), 1);
        assert!(card["image"].as_str().unwrap().starts_with("/api/v1/images/"));
        assert!(acts(&broker).is_empty(), "nothing done before the OK");
        let done = agent
            .app_tool("t1", "app_act", &save(), None, answered(&park, true))
            .await
            .unwrap();
        assert!(done.starts_with("{\"ok\":true"), "{done}");
        let sent = acts(&broker);
        let Request::Act { expect, steps, .. } = &sent[0] else {
            unreachable!()
        };
        assert_eq!((expect.as_slice(), steps.len()), (&[save_binding()][..], 1));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_batch_shows_every_step_and_all_the_text() {
        let (agent, dir, broker) = with_apps();
        let args = json!({"window": "w1", "summary": "Add milk and save", "steps": [
            {"type": "type", "ref": "r7", "text": "- milk\n- eggs ✓"},
            {"type": "keypress", "keys": ["CTRL", "S"]},
        ]});
        let card = park(&agent, &args).await.card;
        assert_eq!(card["action"], "act");
        assert_eq!(card["steps"][0]["action"], "type");
        assert_eq!(card["steps"][0]["text"], "- milk\n- eggs ✓");
        assert_eq!(
            card["steps"][1],
            json!({"action": "key", "key": "CTRL+S", "window": "*notes.txt - Mousepad"})
        );
        // A saved key's value is never typed.
        agent.secrets.put("API_TOKEN", "tok_live_123456789").unwrap();
        let leaky = json!({"window": "w1", "summary": "Paste the token", "steps": [
            {"type": "type", "ref": "r7", "text": "token: tok_live_123456789"}]});
        let refused = agent.app_tool("t1", "app_act", &leaky, None, None).await.unwrap();
        assert!(refused.contains("saved key"), "{refused}");
        // Steps that don't parse say what they should look like.
        let wrong = json!({"window": "w1", "summary": "x", "steps": [{"type": "tap", "ref": "r1"}]});
        let refused = agent.app_tool("t1", "app_act", &wrong, None, None).await.unwrap();
        assert!(refused.contains("Each step is one of"), "{refused}");
        assert!(acts(&broker).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn denied_off_scheduled_or_changed_sends_no_input() {
        let (agent, dir, broker) = with_apps();
        // Denied: nothing more is asked of the app either.
        let park = super::tests::park(&agent, &save()).await;
        let denied = agent
            .app_tool("t1", "app_act", &save(), None, answered(&park, false))
            .await
            .unwrap();
        assert!(denied.starts_with("denied:"), "{denied}");
        assert_eq!(sent(&broker, |r| matches!(r, Request::Prepare { .. })).len(), 1);
        // Approved, but the window changed while the user decided (it was saved).
        let park = super::tests::park(&agent, &save()).await;
        broker.element.lock().unwrap().title = "notes.txt - Mousepad".into();
        let changed = agent
            .app_tool("t1", "app_act", &save(), None, answered(&park, true))
            .await
            .unwrap();
        assert!(changed.starts_with("not done: the app changed"), "{changed}");
        // A scheduled run can't ask; with Browser actions off nobody is asked.
        let scheduled = agent
            .app_tool("t1", "app_act", &save(), Some(3), None)
            .await
            .unwrap();
        assert!(scheduled.contains("can't ask"), "{scheduled}");
        agent.update_settings(None, Some(false)).unwrap().unwrap();
        let off = agent
            .app_tool("t1", "app_act", &save(), None, None)
            .await
            .unwrap();
        assert!(off.starts_with("blocked:"), "{off}");
        assert!(acts(&broker).is_empty());
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
        // A model that doesn't see images: no pixel tools, and screenshots go to the user.
        let gui = agent
            .app_tool("t1", "app_gui", &json!({"actions": []}), None, None)
            .await
            .unwrap();
        assert!(gui.contains("there's no tool"), "{gui}");
        let shown = agent
            .app_tool("t1", "app_screenshot", &json!({}), None, None)
            .await
            .unwrap();
        assert_eq!(shown, "shown to the user");
        assert!(agent.model_images.lock().unwrap().is_empty());
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

    #[tokio::test]
    async fn a_seeing_model_gets_screenshots_and_pixel_cards_mark_its_frame() {
        let (agent, dir, broker) = with_apps();
        agent.vision_tasks.lock().unwrap().insert("t1".into());
        let looked = agent
            .app_tool("t1", "app_screenshot", &json!({}), None, None)
            .await
            .unwrap();
        assert!(
            looked.contains("1229×768") && looked.contains("attached"),
            "{looked}"
        );
        let frame = agent.model_frames.lock().unwrap()["t1"].clone();
        assert_eq!(agent.model_images.lock().unwrap()["t1"], vec![frame.clone()]);
        let args =
            json!({"summary": "Open the File menu", "actions": [{"type": "click", "x": 100, "y": 120}]});
        let park = agent
            .app_tool("t1", "app_gui", &args, None, None)
            .await
            .unwrap_err()
            .downcast::<Park>()
            .expect("pixel input asks first");
        let card = &park.card;
        assert_eq!(
            (card["action"].as_str(), card["app"].as_str()),
            (Some("gui"), Some("Mousepad"))
        );
        assert_eq!(card["image"], format!("/api/v1/images/{frame}"));
        assert_eq!(card["frame"], json!({"width": 1229, "height": 768}));
        assert_eq!(card["steps"][0]["action"], "click");
        assert!(card["steps"][0].get("type").is_none(), "{card}");
        assert_eq!(
            (
                card["steps"][0]["x"].as_f64(),
                card["steps"][0]["window"].as_str()
            ),
            (Some(100.0), Some("notes.txt - Mousepad"))
        );
        assert!(acts(&broker).is_empty());
        let done = agent
            .app_tool("t1", "app_gui", &args, None, answered(&park, true))
            .await
            .unwrap();
        assert!(done.contains("attached after these results"), "{done}");
        assert_eq!(acts(&broker).len(), 1);
        // The screenshot after it is the model's next frame.
        assert_ne!(agent.model_frames.lock().unwrap()["t1"], frame);
        std::fs::remove_dir_all(dir).unwrap();
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
