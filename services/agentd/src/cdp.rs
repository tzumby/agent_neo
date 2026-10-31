//! Minimal Chrome DevTools Protocol client: Chrome's HTTP endpoints for tabs,
//! and one WebSocket per page for commands. Events are ignored.

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
use tokio::sync::{Mutex, oneshot};
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
    sink: Mutex<Sink>,
    pending: Pending,
    next_id: AtomicU64,
    reader: tokio::task::JoinHandle<()>,
}

impl Page {
    pub async fn connect(target: &Target) -> Result<Self> {
        if target.ws_url.is_empty() {
            bail!("tab {} is already being debugged elsewhere", target.id);
        }
        let (ws, _) = tokio_tungstenite::connect_async(target.ws_url.as_str())
            .await
            .with_context(|| format!("connecting to tab {}", target.id))?;
        let (sink, mut stream) = ws.split();
        let pending: Pending = Arc::default();
        let replies = pending.clone();
        let reader = tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                let Message::Text(text) = message else { continue };
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                let Some(id) = value["id"].as_u64() else { continue }; // an event
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
            sink: Mutex::new(sink),
            pending,
            next_id: AtomicU64::new(1),
            reader,
        };
        // Needed for node IDs, quads and the accessibility tree.
        for domain in ["Page.enable", "DOM.enable", "Accessibility.enable"] {
            page.call(domain, json!({})).await?;
        }
        Ok(page)
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
