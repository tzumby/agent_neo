//! Minimal Chrome DevTools Protocol client: Chrome's HTTP endpoints for tabs,
//! and one WebSocket per page for commands; its events go to whoever listens.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

#[derive(Clone, Debug, Deserialize)]
pub struct Target {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub ws_url: String,
}

#[derive(Clone)]
pub struct Cdp {
    http: reqwest::Client,
    base: String,
}

impl Cdp {
    pub fn new(base: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    pub async fn pages(&self) -> Result<Vec<Target>> {
        let targets: Vec<Target> = self
            .http
            .get(format!("{}/json/list", self.base))
            .send()
            .await
            .context("Chrome isn't reachable")?
            .error_for_status()?
            .json()
            .await?;
        Ok(targets.into_iter().filter(|t| t.kind == "page").collect())
    }

    pub async fn new_page(&self) -> Result<Target> {
        // Chrome requires PUT here. Navigate afterwards, so URLs need no escaping.
        let target = self
            .http
            .put(format!("{}/json/new?about:blank", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(target)
    }

    /// Commands on the browser's own session (not a tab's), where windows
    /// are opened and found. One reply per call, in the order asked.
    async fn browser_calls(&self, calls: &[(&str, Value)]) -> Result<Vec<Result<Value>>> {
        #[derive(Deserialize)]
        struct Version {
            #[serde(rename = "webSocketDebuggerUrl")]
            ws_url: String,
        }
        let version: Version = self
            .http
            .get(format!("{}/json/version", self.base))
            .send()
            .await
            .context("Chrome isn't reachable")?
            .error_for_status()?
            .json()
            .await?;
        let (mut ws, _) = tokio_tungstenite::connect_async(version.ws_url.as_str())
            .await
            .context("connecting to Chrome")?;
        for (i, (method, params)) in calls.iter().enumerate() {
            let request = json!({"id": i + 1, "method": method, "params": params});
            ws.send(Message::Text(request.to_string().into())).await?;
        }
        let mut replies: Vec<Option<Result<Value>>> = calls.iter().map(|_| None).collect();
        let answered = tokio::time::timeout(Duration::from_secs(10), async {
            let mut left = calls.len();
            while left > 0 {
                let Some(message) = ws.next().await else {
                    bail!("Chrome closed the connection")
                };
                if let Message::Text(text) = message?
                    && let Ok(value) = serde_json::from_str::<Value>(text.as_str())
                    && let Some(slot) = value["id"]
                        .as_u64()
                        .and_then(|id| replies.get_mut((id as usize).wrapping_sub(1)))
                    && slot.is_none()
                {
                    *slot = Some(match value.get("error") {
                        Some(err) => Err(anyhow!("{}", err["message"].as_str().unwrap_or("DevTools error"))),
                        None => Ok(value["result"].clone()),
                    });
                    left -= 1;
                }
            }
            Ok(())
        })
        .await;
        let _ = ws.close(None).await;
        answered.map_err(|_| anyhow!("Chrome didn't answer in time"))??;
        Ok(replies
            .into_iter()
            .map(|reply| reply.unwrap_or_else(|| Err(anyhow!("no reply"))))
            .collect())
    }

    /// A new window with one blank tab, not brought to the front: a scheduled
    /// run's or a conversation's, so it never takes the tab the chat works in.
    pub async fn new_window(&self) -> Result<Target> {
        let params = json!({"url": "about:blank", "newWindow": true, "background": true});
        let reply = self
            .browser_calls(&[("Target.createTarget", params)])
            .await?
            .pop()
            .ok_or_else(|| anyhow!("no reply"))?
            .map_err(|err| anyhow!("Chrome didn't open a window: {err}"))?;
        let id = reply["targetId"]
            .as_str()
            .ok_or_else(|| anyhow!("Chrome didn't say which window it opened"))?
            .to_string();
        self.pages()
            .await?
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| anyhow!("Chrome's new window isn't listed"))
    }

    /// The window each of these tabs is in, by tab id; tabs that are gone
    /// are left out.
    pub async fn windows(&self, ids: &[String]) -> Result<HashMap<String, i64>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let calls: Vec<(&str, Value)> = ids
            .iter()
            .map(|id| ("Browser.getWindowForTarget", json!({"targetId": id})))
            .collect();
        let replies = self.browser_calls(&calls).await?;
        Ok(ids
            .iter()
            .zip(replies)
            .filter_map(|(id, reply)| Some((id.clone(), reply.ok()?["windowId"].as_i64()?)))
            .collect())
    }

    /// Closes the window `id` is in: every tab in it (a conversation's, when
    /// it's archived or deleted).
    pub async fn close_window_of(&self, id: &str) -> Result<()> {
        let pages = self.pages().await?;
        let ids: Vec<String> = pages.iter().map(|t| t.id.clone()).collect();
        let tabs = match self.windows(&ids).await {
            Ok(windows) => match windows.get(id) {
                Some(window) => ids
                    .into_iter()
                    .filter(|tab| windows.get(tab) == Some(window))
                    .collect(),
                None => Vec::new(), // gone already
            },
            Err(_) => vec![id.to_string()],
        };
        for tab in tabs {
            self.close(&tab).await?;
        }
        Ok(())
    }

    /// Closes a tab (the last one closes its window).
    pub async fn close(&self, id: &str) -> Result<()> {
        self.http
            .get(format!("{}/json/close/{id}", self.base))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn activate(&self, id: &str) -> Result<()> {
        self.http
            .get(format!("{}/json/activate/{id}", self.base))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

type Sink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type Pending = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// A command session with one page (tab).
pub struct Page {
    pub target_id: String,
    sink: Arc<Mutex<Sink>>,
    pending: Pending,
    /// DevTools events from the tab (network requests and so on).
    events: broadcast::Sender<Value>,
    next_id: Arc<AtomicU64>,
    /// What the page did on its own that the agent should hear about (a
    /// dialog it answered, a file picker held back), until `take_notes`.
    notes: Arc<StdMutex<Vec<String>>>,
    reader: tokio::task::JoinHandle<()>,
}

/// How agentd answers a dialog the page opens (`alert`, `confirm`, `prompt`,
/// `beforeunload`): whether to accept it, and what to tell the model. While a
/// dialog is open the page is frozen and every DevTools call to it waits, so
/// it's answered at once. Closing a message box and leaving a page the agent
/// asked to leave are what it wanted; OK on "Are you sure?" would be a second
/// action nobody approved, so that's cancelled and the model told.
pub fn answer_dialog(kind: &str, message: &str) -> (bool, String) {
    let message: String = message.trim().chars().take(300).collect();
    match kind {
        "alert" => (
            true,
            format!("the page showed a message box: \"{message}\"; agentd closed it"),
        ),
        "beforeunload" => (
            true,
            "the page warned that leaving could lose unsaved changes; agentd left it, as you asked".into(),
        ),
        "prompt" => (
            false,
            format!(
                "the page asked for input in a pop-up (\"{message}\"); agentd cancelled it, so what it was \
                 about to do didn't happen"
            ),
        ),
        _ => (
            false,
            format!(
                "the page asked \"{message}\" (OK or Cancel); agentd answered Cancel, so what it was about to do \
                 didn't happen. If it matters, tell the user: they can do it through Take over"
            ),
        ),
    }
}

/// What the model hears when the page tries to open a file picker.
pub const FILE_PICKER_NOTE: &str = "the page opened a file picker; agentd can't choose files, so nothing was \
     attached. If a file is needed, tell the user to attach it through Take over";

impl Page {
    pub async fn connect(target: &Target) -> Result<Self> {
        if target.ws_url.is_empty() {
            bail!("tab {} is already being debugged elsewhere", target.id);
        }
        let (ws, _) = tokio_tungstenite::connect_async(target.ws_url.as_str())
            .await
            .with_context(|| format!("connecting to tab {}", target.id))?;
        let (sink, mut stream) = ws.split();
        let sink = Arc::new(Mutex::new(sink));
        let pending: Pending = Arc::default();
        let replies = pending.clone();
        let (events, _) = broadcast::channel(4096);
        let notify = events.clone();
        let next_id = Arc::new(AtomicU64::new(1));
        let notes: Arc<StdMutex<Vec<String>>> = Arc::default();
        let (answer_sink, answer_ids, noted) = (sink.clone(), next_id.clone(), notes.clone());
        let reader = tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                let Message::Text(text) = message else { continue };
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                let Some(id) = value["id"].as_u64() else {
                    match value["method"].as_str() {
                        // Answered here, at once: the page waits for it.
                        Some("Page.javascriptDialogOpening") => {
                            let params = &value["params"];
                            let (accept, note) = answer_dialog(
                                params["type"].as_str().unwrap_or(""),
                                params["message"].as_str().unwrap_or(""),
                            );
                            let id = answer_ids.fetch_add(1, Ordering::Relaxed);
                            let answer = json!({"id": id, "method": "Page.handleJavaScriptDialog",
                                                "params": {"accept": accept}});
                            let _ = answer_sink
                                .lock()
                                .await
                                .send(Message::Text(answer.to_string().into()))
                                .await;
                            noted.lock().unwrap().push(note);
                        }
                        Some("Page.fileChooserOpened") => noted.lock().unwrap().push(FILE_PICKER_NOTE.into()),
                        _ => {}
                    }
                    // An event: for whoever listens (nobody, most of the time).
                    let _ = notify.send(value);
                    continue;
                };
                let Some(reply) = replies.lock().unwrap().remove(&id) else {
                    continue;
                };
                let result = match value.get("error") {
                    Some(err) => Err(anyhow!("{}", err["message"].as_str().unwrap_or("DevTools error"))),
                    None => Ok(value["result"].clone()),
                };
                let _ = reply.send(result);
            }
            for (_, reply) in replies.lock().unwrap().drain() {
                let _ = reply.send(Err(anyhow!("the tab's connection closed")));
            }
        });
        let page = Self {
            target_id: target.id.clone(),
            sink,
            pending,
            events,
            next_id,
            notes,
            reader,
        };
        // Node IDs, quads and the accessibility tree; network events tell
        // when a page has finished loading.
        for domain in [
            "Page.enable",
            "DOM.enable",
            "Accessibility.enable",
            "Network.enable",
        ] {
            page.call(domain, json!({})).await?;
        }
        // DOMContentLoaded, first paint and load for each document: what
        // agentd waits on after a navigation (browser.rs: PageWatch).
        page.call("Page.setLifecycleEventsEnabled", json!({"enabled": true}))
            .await?;
        Ok(page)
    }

    /// Listens to the tab's events from now on.
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// What the page did on its own since the last call (dialogs answered,
    /// file pickers held back), for the model.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().unwrap())
    }

    /// While on, a file picker the page opens is held back (no system dialog,
    /// which the agent couldn't see) and noted instead. Only around the
    /// agent's own clicks and keys, so the user's still open in Take over.
    pub async fn hold_file_pickers(&self, on: bool) {
        // Best effort: an older Chrome without it still opens the dialog.
        let _ = self
            .call("Page.setInterceptFileChooserDialog", json!({"enabled": on}))
            .await;
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, reply);
        let request = json!({"id": id, "method": method, "params": params}).to_string();
        self.sink.lock().await.send(Message::Text(request.into())).await?;
        match tokio::time::timeout(Duration::from_secs(30), response).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => bail!("the tab's connection closed"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("{method} timed out")
            }
        }
    }

    pub async fn eval(&self, expression: &str) -> Result<Value> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            bail!("{}", exception["text"].as_str().unwrap_or("JavaScript exception"));
        }
        Ok(result["result"]["value"].clone())
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialogs_are_answered_as_the_agent_would_want() {
        assert!(answer_dialog("alert", "Saved").0);
        assert!(answer_dialog("beforeunload", "").0);
        let (ok, note) = answer_dialog("confirm", "Delete all 12 messages?");
        assert!(!ok && note.contains("\"Delete all 12 messages?\"") && note.contains("Cancel"));
        assert!(!answer_dialog("prompt", "Your name").0);
        assert!(!answer_dialog("something new", "?").0); // unknown kinds: cancelled
        let long = answer_dialog("alert", &"y".repeat(1000)).1;
        assert!(long.contains(&"y".repeat(300)) && !long.contains(&"y".repeat(301)));
    }

    /// Against a real Chrome: `CHROME_CDP=http://127.0.0.1:9333 cargo test --
    /// --ignored page_dialogs_and_file_pickers` with a headless Chrome on that
    /// debugging port.
    #[tokio::test]
    #[ignore]
    async fn page_dialogs_and_file_pickers() {
        let cdp = Cdp::new(&std::env::var("CHROME_CDP").expect("CHROME_CDP")).unwrap();
        let page = Page::connect(&cdp.new_page().await.unwrap()).await.unwrap();
        let html =
            "<input type=file id=f style='position:absolute;left:10px;top:10px;width:200px;height:40px'>";
        page.call("Page.navigate", json!({"url": format!("data:text/html,{html}")}))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        // A pop-up freezes the page until it's answered: these return at once.
        assert_eq!(
            page.eval("confirm('Delete everything?')").await.unwrap(),
            json!(false)
        );
        assert_eq!(page.eval("alert('Saved'); 1").await.unwrap(), json!(1));
        // A real click on the file input, with pickers held back: no dialog, a note.
        page.hold_file_pickers(true).await;
        for kind in ["mousePressed", "mouseReleased"] {
            page.call(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": 100, "y": 30, "button": "left", "clickCount": 1}),
            )
            .await
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        page.hold_file_pickers(false).await;
        let notes = page.take_notes();
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[0].contains("answered Cancel") && notes[1].contains("closed it"));
        assert_eq!(notes[2], FILE_PICKER_NOTE);
        assert!(page.take_notes().is_empty());
    }
}
