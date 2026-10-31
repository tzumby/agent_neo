//! Browser tools on the logged-in Chrome, through the DevTools Protocol.
//! Elements are found through the accessibility tree; `ref`s in snapshots
//! are Chrome's backend DOM node IDs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::cdp::{Cdp, Page};
use crate::llm::truncate;
use crate::policy::{Gate, Policy};

/// An element a tool call refers to, found in the accessibility tree.
pub struct Element {
    node: i64,
    pub role: Option<String>,
    pub label: String,
}

pub struct Browser {
    cdp: Cdp,
    page: Option<Page>,
    allow_actions: bool,
    policy: Arc<Policy>,
}

impl Browser {
    pub fn new(cdp: Cdp, allow_actions: bool, policy: Arc<Policy>) -> Self {
        Self {
            cdp,
            page: None,
            allow_actions,
            policy,
        }
    }

    /// The current tab: the one remembered from earlier, else the first tab.
    pub async fn attach(&mut self, remembered: Option<&str>) -> Result<()> {
        let pages = self.cdp.pages().await?;
        let target = match pages.iter().find(|t| Some(t.id.as_str()) == remembered) {
            Some(t) => t.clone(),
            None => match pages.first() {
                Some(t) => t.clone(),
                None => self.cdp.new_page().await?,
            },
        };
        self.page = Some(Page::connect(&target).await?);
        Ok(())
    }

    pub fn current_target(&self) -> Option<String> {
        self.page.as_ref().map(|p| p.target_id.clone())
    }

    fn page(&self) -> Result<&Page> {
        self.page.as_ref().ok_or_else(|| anyhow!("no tab is open"))
    }

    async fn switch_to(&mut self, target: &crate::cdp::Target) -> Result<()> {
        self.cdp.activate(&target.id).await?;
        self.page = Some(Page::connect(target).await?);
        Ok(())
    }

    pub async fn where_am_i(&self) -> Result<String> {
        let value = self
            .page()?
            .eval("JSON.stringify([document.title, location.href])")
            .await?;
        let pair: (String, String) = serde_json::from_str(value.as_str().unwrap_or("[\"\",\"\"]"))?;
        Ok(format!("{} | {}", pair.0, pair.1))
    }

    /// A heuristic: the open tab shows a visible password field or has a sign-in URL.
    pub async fn looks_like_sign_in(&self) -> bool {
        let Ok(page) = self.page() else { return false };
        page.eval(SIGN_IN_CHECK)
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Starts listening to the tab's network events. Call before the action
    /// whose effects `settle` should wait for, so no request is missed.
    fn watch(&self) -> Result<NetworkWatch> {
        Ok(NetworkWatch::new(self.page()?.events()))
    }

    /// Waits until the page has settled, up to `max_ms`: the DOM has stopped
    /// changing (a MutationObserver in the page, no loading indicator) and the
    /// network is quiet (the tab's DevTools request events). Single-page apps
    /// such as Slack sit still while their code and data download, so the DOM
    /// alone isn't enough. All on events; nothing polls. Returns how it ended.
    async fn settle(&self, mut network: NetworkWatch, max_ms: u64) -> Result<String> {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(max_ms);
        let took = |how: &str| format!("{how} after {} ms", started.elapsed().as_millis());
        loop {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1);
            let dom = self
                .eval_across_navigation(&DOM_QUIET.replace("MAX_MS", &left.to_string()))
                .await?;
            let dom_quiet = Instant::now();
            if dom != "quiet" {
                return Ok(took("still changing, stopped waiting"));
            }
            if !network.idle(deadline).await {
                return Ok(took("still loading, stopped waiting"));
            }
            // Requests that finished since the DOM went quiet may still render.
            if !network.active_since(dom_quiet) {
                return Ok(took("settled"));
            }
        }
    }

    /// Evaluates a promise in the page. A click can start a navigation that
    /// destroys the page's JavaScript context mid-wait; then evaluate again in
    /// the new document.
    async fn eval_across_navigation(&self, script: &str) -> Result<String> {
        let mut last = None;
        for _ in 0..3 {
            match self.page()?.eval(script).await {
                Ok(value) => return Ok(value.as_str().unwrap_or_default().to_string()),
                Err(err) => {
                    let text = err.to_string();
                    if !(text.contains("context") || text.contains("navigated")) {
                        return Err(err);
                    }
                    last = Some(err);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("the page kept navigating")))
    }

    /// The wait_for tool: until `text` shows on the page (or, without text,
    /// until the page settles), reacting to DOM changes; never polling.
    pub async fn wait_for(&self, text: Option<&str>, timeout_secs: u64) -> Result<String> {
        let max_ms = timeout_secs.clamp(1, 25) * 1000;
        let outcome = match text.map(str::trim).filter(|t| !t.is_empty()) {
            None => self.settle(self.watch()?, max_ms).await?,
            Some(text) => {
                let script = WAIT_FOR_TEXT
                    .replace("TEXT_JSON", &serde_json::to_string(&text.to_lowercase())?)
                    .replace("MAX_MS", &max_ms.to_string());
                self.eval_across_navigation(&script).await?
            }
        };
        Ok(format!("{outcome}; now at {}", self.where_am_i().await?))
    }

    // --- Tools ---------------------------------------------------------------

    pub async fn list_tabs(&self) -> Result<String> {
        let current = self.current_target();
        let pages = self.cdp.pages().await?;
        Ok(pages
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mark = if Some(&t.id) == current.as_ref() { "*" } else { " " };
                format!("{i}{mark} {} | {}", truncate(&t.title, 80), truncate(&t.url, 150))
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    pub async fn switch_tab(&mut self, index: usize) -> Result<String> {
        let pages = self.cdp.pages().await?;
        let target = pages
            .get(index)
            .ok_or_else(|| anyhow!("there is no tab {index}"))?
            .clone();
        self.switch_to(&target).await?;
        Ok(format!("current tab: {}", self.where_am_i().await?))
    }

    pub async fn open_url(&mut self, url: &str, new_tab: bool) -> Result<String> {
        if new_tab {
            let target = self.cdp.new_page().await?;
            self.switch_to(&target).await?;
        }
        let network = self.watch()?;
        let result = self.page()?.call("Page.navigate", json!({"url": url})).await?;
        if let Some(error) = result["errorText"].as_str() {
            bail!("couldn't load {url}: {error}");
        }
        let settled = self.settle(network, 15_000).await?;
        Ok(format!("{} ({settled})", self.where_am_i().await?))
    }

    pub async fn read_text(&self, max_chars: usize) -> Result<String> {
        let text = self
            .page()?
            .eval("document.body ? document.body.innerText : ''")
            .await?;
        Ok(truncate(text.as_str().unwrap_or(""), max_chars))
    }

    pub async fn snapshot(&self, max_chars: usize) -> Result<String> {
        let nodes = self.ax_nodes().await?;
        Ok(truncate(&format_snapshot(&nodes), max_chars))
    }

    /// Finds the element a click or type_text call means.
    pub async fn find(
        &self,
        reference: Option<i64>,
        role: Option<&str>,
        name: Option<&str>,
        text: Option<&str>,
    ) -> Result<Element> {
        let nodes = self.ax_nodes().await?;
        let (node, role, label) = resolve(&nodes, reference, role, name, text)?;
        Ok(Element { node, role, label })
    }

    pub fn click_gate(&self, element: &Element) -> Gate {
        self.policy
            .click_gate(self.allow_actions, element.role.as_deref(), &element.label)
    }

    pub fn type_gate(&self, element: &Element, submit: bool) -> Gate {
        self.policy.type_gate(
            self.allow_actions,
            element.role.as_deref(),
            &element.label,
            submit,
        )
    }

    pub fn key_gate(&self, key: &str) -> Gate {
        self.policy.key_gate(self.allow_actions, key)
    }

    pub async fn click(&self, element: &Element) -> Result<String> {
        let network = self.watch()?;
        self.click_node(element.node).await?;
        let settled = self.settle(network, 10_000).await?;
        Ok(format!(
            "clicked '{}' ({settled}); now at {}",
            element.label,
            self.where_am_i().await?
        ))
    }

    pub async fn type_text(&self, element: &Element, text: &str, submit: bool) -> Result<String> {
        let page = self.page()?;
        page.call("DOM.focus", json!({"backendNodeId": element.node}))
            .await?;
        page.call("Input.insertText", json!({"text": text})).await?;
        if submit {
            let network = self.watch()?;
            self.key("Enter").await?;
            self.settle(network, 10_000).await?;
        }
        Ok(format!(
            "typed into '{}'; now at {}",
            element.label,
            self.where_am_i().await?
        ))
    }

    pub async fn press_key(&self, key: &str) -> Result<String> {
        // Keys can send, open or scroll in more content.
        let network = self.watch()?;
        self.key(key).await?;
        let settled = self.settle(network, 6_000).await?;
        Ok(format!("pressed ({settled})"))
    }

    /// The open page's host, for approval cards (e.g. app.slack.com).
    pub async fn host(&self) -> String {
        match self.page() {
            Ok(page) => page
                .eval("location.hostname")
                .await
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// What has the keyboard focus (its label), for approving a key press.
    pub async fn focused(&self) -> String {
        const FOCUSED: &str = "(() => { const e = document.activeElement; if (!e || e === document.body) return ''; \
            return e.getAttribute('aria-label') || e.getAttribute('placeholder') || e.getAttribute('name') || ''; })()";
        match self.page() {
            Ok(page) => page
                .eval(FOCUSED)
                .await
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    pub async fn screenshot_jpeg(&self) -> Result<Vec<u8>> {
        let shot = self
            .page()?
            .call("Page.captureScreenshot", json!({"format": "jpeg", "quality": 70}))
            .await?;
        let data = shot["data"]
            .as_str()
            .ok_or_else(|| anyhow!("no screenshot data"))?;
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }

    // --- Helpers ---------------------------------------------------------------

    async fn ax_nodes(&self) -> Result<Vec<Value>> {
        let tree = self
            .page()?
            .call("Accessibility.getFullAXTree", json!({}))
            .await?;
        Ok(tree["nodes"].as_array().cloned().unwrap_or_default())
    }

    async fn click_node(&self, backend_id: i64) -> Result<()> {
        let page = self.page()?;
        let _ = page
            .call("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": backend_id}))
            .await;
        let quads = page
            .call("DOM.getContentQuads", json!({"backendNodeId": backend_id}))
            .await?;
        let quad: Vec<f64> = quads["quads"][0]
            .as_array()
            .context("the element isn't visible")?
            .iter()
            .filter_map(Value::as_f64)
            .collect();
        if quad.len() != 8 {
            bail!("the element has no clickable area");
        }
        let x = (quad[0] + quad[2] + quad[4] + quad[6]) / 4.0;
        let y = (quad[1] + quad[3] + quad[5] + quad[7]) / 4.0;
        for (kind, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            page.call(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "buttons": buttons, "clickCount": 1}),
            )
            .await?;
        }
        Ok(())
    }

    async fn key(&self, key: &str) -> Result<()> {
        let (key_name, code, key_code, text) = key_definition(key)?;
        let page = self.page()?;
        let mut down =
            json!({"type": "keyDown", "key": key_name, "code": code, "windowsVirtualKeyCode": key_code});
        if let Some(text) = text {
            down["text"] = json!(text);
        }
        page.call("Input.dispatchKeyEvent", down).await?;
        page.call(
            "Input.dispatchKeyEvent",
            json!({"type": "keyUp", "key": key_name, "code": code, "windowsVirtualKeyCode": key_code}),
        )
        .await?;
        Ok(())
    }
}

fn key_definition(key: &str) -> Result<(String, String, i64, Option<String>)> {
    let named =
        |code: i64, text: Option<&str>| Ok((key.to_string(), key.to_string(), code, text.map(String::from)));
    match key {
        "Enter" => named(13, Some("\r")),
        "Tab" => named(9, None),
        "Escape" => named(27, None),
        "Backspace" => named(8, None),
        "Delete" => named(46, None),
        "PageUp" => named(33, None),
        "PageDown" => named(34, None),
        "End" => named(35, None),
        "Home" => named(36, None),
        "ArrowLeft" => named(37, None),
        "ArrowUp" => named(38, None),
        "ArrowRight" => named(39, None),
        "ArrowDown" => named(40, None),
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    let code = c.to_ascii_uppercase() as i64;
                    Ok((c.to_string(), String::new(), code, Some(c.to_string())))
                }
                _ => bail!("unknown key {key}"),
            }
        }
    }
}

// --- Accessibility tree ---------------------------------------------------------

/// Roles that only add structure; their children are shown in their place.
/// Resolves "quiet" once the document has loaded and the DOM has had no
/// relevant change for 600 ms with no loading indicator showing, or "busy"
/// after MAX_MS. On the page's events (DOMContentLoaded, MutationObserver);
/// the timers only measure quiet.
const DOM_QUIET: &str = r#"(async () => {
  const started = Date.now(), max = MAX_MS, quietMs = 600;
  if (document.readyState === "loading") {
    await new Promise((done) => document.addEventListener("DOMContentLoaded", done, { once: true }));
  }
  const busy = () => !!document.querySelector('[aria-busy="true"], [role="progressbar"]:not([aria-hidden="true"])');
  return await new Promise((resolve) => {
    let quiet;
    const finish = (how) => { observer.disconnect(); clearTimeout(quiet); clearTimeout(cap); resolve(how); };
    // When quiet but a loading indicator shows, wait for the next change.
    const arm = () => { clearTimeout(quiet); quiet = setTimeout(() => { if (!busy()) finish("quiet"); }, quietMs); };
    const observer = new MutationObserver(arm);
    observer.observe(document.documentElement, {
      subtree: true, childList: true, characterData: true,
      attributes: true, attributeFilter: ["aria-busy", "hidden", "aria-hidden", "disabled", "src", "value"],
    });
    const cap = setTimeout(() => finish("busy"), Math.max(0, max - (Date.now() - started)));
    arm();
  });
})()"#;

/// Resolves when TEXT_JSON appears in the page's text (checked when the DOM
/// changes, at most every 150 ms), or after MAX_MS.
const WAIT_FOR_TEXT: &str = r#"(async () => {
  const started = Date.now(), text = TEXT_JSON;
  const has = () => (document.body?.innerText ?? "").toLowerCase().includes(text);
  if (has()) return "found it right away";
  return await new Promise((resolve) => {
    let pending = false;
    const finish = (how) => { observer.disconnect(); clearTimeout(cap); resolve(how); };
    const observer = new MutationObserver(() => {
      if (pending) return;
      pending = true;
      setTimeout(() => { pending = false; if (has()) finish(`found it after ${Date.now() - started} ms`); }, 150);
    });
    observer.observe(document.documentElement, { subtree: true, childList: true, characterData: true });
    const cap = setTimeout(() => finish(`not on the page after ${MAX_MS} ms`), MAX_MS);
  });
})()"#;

/// How long the network must be quiet before a page counts as loaded.
const NETWORK_QUIET: Duration = Duration::from_millis(500);

/// Requests in flight on a tab, from its DevTools network events.
struct NetworkWatch {
    events: broadcast::Receiver<Value>,
    in_flight: HashSet<String>,
    last_activity: Instant,
}

impl NetworkWatch {
    fn new(events: broadcast::Receiver<Value>) -> Self {
        Self {
            events,
            in_flight: HashSet::new(),
            last_activity: Instant::now(),
        }
    }

    fn apply(&mut self, event: &Value) {
        let params = &event["params"];
        let id = params["requestId"].as_str().unwrap_or_default().to_string();
        match event["method"].as_str() {
            Some("Network.requestWillBeSent") => {
                // Live connections (event streams) never finish; don't wait on them.
                if !matches!(
                    params["type"].as_str(),
                    Some("EventSource" | "WebSocket" | "Ping")
                ) {
                    self.in_flight.insert(id);
                }
                self.last_activity = Instant::now();
            }
            Some("Network.loadingFinished" | "Network.loadingFailed") => {
                self.in_flight.remove(&id);
                self.last_activity = Instant::now();
            }
            _ => {}
        }
    }

    fn catch_up(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(event) => self.apply(&event),
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    }

    /// Waits until at most one request is in flight (a long poll, say) and
    /// nothing has started or finished for NETWORK_QUIET. False at `deadline`.
    async fn idle(&mut self, deadline: Instant) -> bool {
        loop {
            self.catch_up();
            let now = Instant::now();
            let quiet_for = now.saturating_duration_since(self.last_activity);
            let calm = self.in_flight.len() <= 1;
            if calm && quiet_for >= NETWORK_QUIET {
                return true;
            }
            if now >= deadline {
                return false;
            }
            let wait = if calm {
                NETWORK_QUIET - quiet_for
            } else {
                deadline - now
            };
            match tokio::time::timeout(wait.min(deadline - now), self.events.recv()).await {
                Ok(Ok(event)) => self.apply(&event),
                Ok(Err(broadcast::error::RecvError::Closed)) => return true,
                // Lagged, or the quiet period ran out: look again.
                _ => {}
            }
        }
    }

    fn active_since(&mut self, moment: Instant) -> bool {
        self.catch_up();
        self.last_activity > moment
    }
}

const SIGN_IN_CHECK: &str = r#"(() => {
  const visible = (el) => el.getClientRects().length > 0;
  if ([...document.querySelectorAll('input[type=password]')].some(visible)) return true;
  return /^(accounts|login|auth|signin|sso)\./i.test(location.hostname)
    || /\/(login|log-in|signin|sign-in|sign_in|session|auth|sso)(\/|$)/i.test(location.pathname);
})()"#;

const SKIPPED_ROLES: &[&str] = &[
    "generic",
    "none",
    "GenericContainer",
    "InlineTextBox",
    "LineBreak",
    "RootWebArea",
];
/// Roles worth matching first when looking an element up by its text.
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "menuitem",
    "tab",
    "checkbox",
    "radio",
    "option",
    "textbox",
    "searchbox",
    "combobox",
    "switch",
    "row",
    "treeitem",
];

fn ax_str(node: &Value, field: &str) -> String {
    node[field]["value"].as_str().unwrap_or("").to_string()
}

fn ignored(node: &Value) -> bool {
    node["ignored"].as_bool().unwrap_or(false)
}

/// One line per meaningful node: `- role "name" [ref=N]`, indented by depth.
pub fn format_snapshot(nodes: &[Value]) -> String {
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| n["nodeId"].as_str().map(|id| (id, n)))
        .collect();
    let mut out = String::new();
    let Some(root) = nodes.first() else { return out };
    let mut stack: Vec<(&Value, usize)> = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        let role = ax_str(node, "role");
        let name = ax_str(node, "name");
        let shown = !ignored(node) && !SKIPPED_ROLES.contains(&role.as_str());
        let child_depth = if shown { depth + 1 } else { depth };
        if shown {
            let indent = "  ".repeat(depth);
            if role == "StaticText" {
                if !name.trim().is_empty() {
                    out.push_str(&format!("{indent}- text: {}\n", name.trim()));
                }
            } else {
                let reference = node["backendDOMNodeId"]
                    .as_i64()
                    .map(|id| format!(" [ref={id}]"))
                    .unwrap_or_default();
                let label = if name.is_empty() {
                    String::new()
                } else {
                    format!(" \"{}\"", name.trim())
                };
                out.push_str(&format!("{indent}- {role}{label}{reference}\n"));
            }
        }
        if let Some(children) = node["childIds"].as_array() {
            for child in children.iter().rev() {
                if let Some(child) = child.as_str().and_then(|id| by_id.get(id)) {
                    stack.push((child, child_depth));
                }
            }
        }
    }
    out
}

/// Finds the element to act on: by ref, by role and name, or by visible text.
/// Returns (backend node ID, role, label).
fn resolve(
    nodes: &[Value],
    reference: Option<i64>,
    role: Option<&str>,
    name: Option<&str>,
    text: Option<&str>,
) -> Result<(i64, Option<String>, String)> {
    let live = || {
        nodes
            .iter()
            .filter(|n| !ignored(n) && n["backendDOMNodeId"].as_i64().is_some())
    };
    let pick = |n: &Value| {
        let role = ax_str(n, "role");
        let label = ax_str(n, "name");
        (n["backendDOMNodeId"].as_i64().unwrap(), Some(role), label)
    };
    if let Some(id) = reference {
        return Ok(nodes
            .iter()
            .find(|n| n["backendDOMNodeId"].as_i64() == Some(id))
            .map(pick)
            .unwrap_or((id, None, format!("ref {id}"))));
    }
    if let (Some(role), Some(name)) = (role, name) {
        let (role, name) = (role.to_lowercase(), name.to_lowercase());
        let matches_role = |n: &&Value| ax_str(n, "role").to_lowercase() == role;
        if let Some(n) = live()
            .filter(matches_role)
            .find(|n| ax_str(n, "name").to_lowercase() == name)
        {
            return Ok(pick(n));
        }
        if let Some(n) = live()
            .filter(matches_role)
            .find(|n| ax_str(n, "name").to_lowercase().contains(&name))
        {
            return Ok(pick(n));
        }
        bail!("no {role} named \"{name}\" on the page (try snapshot)");
    }
    if let Some(text) = text {
        let text = text.to_lowercase();
        let contains = |n: &&Value| ax_str(n, "name").to_lowercase().contains(&text);
        let interactive = |n: &&Value| INTERACTIVE_ROLES.contains(&ax_str(n, "role").as_str());
        if let Some(n) = live()
            .filter(interactive)
            .find(contains)
            .or_else(|| live().find(contains))
        {
            return Ok(pick(n));
        }
        bail!("no element with the text \"{text}\" on the page (try snapshot)");
    }
    bail!("give ref, or role and name, or text")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens SETTLE_URL in a new tab of a real Chrome (CDP_URL, e.g. through
    /// `make cdp`) and checks that SETTLE_TEXT is on the page as soon as
    /// open_url returns. Prints the tab's id so it can be closed afterwards.
    #[tokio::test]
    #[ignore = "needs CDP_URL pointing at a Chrome"]
    async fn live_open_url_waits_for_the_page_to_settle() {
        let cdp = Cdp::new(&std::env::var("CDP_URL").expect("CDP_URL")).unwrap();
        let target = cdp.new_page().await.unwrap();
        println!("tab {}", target.id);
        let mut browser = Browser::new(cdp.clone(), false, Arc::new(Policy::defaults()));
        browser.switch_to(&target).await.unwrap();
        let url = std::env::var("SETTLE_URL").unwrap_or_else(|_| "https://example.com".into());
        let expect = std::env::var("SETTLE_TEXT").unwrap_or_else(|_| "documentation".into());
        let started = Instant::now();
        let result = browser.open_url(&url, false).await.unwrap();
        let text = browser.read_text(500_000).await.unwrap().to_lowercase();
        println!(
            "{result}\nreturned after {:?}; page has {expect:?}: {}",
            started.elapsed(),
            text.contains(&expect.to_lowercase())
        );
        assert!(text.contains(&expect.to_lowercase()));
    }

    fn tree() -> Vec<Value> {
        serde_json::from_str(
            r#"[
            {"nodeId":"1","ignored":false,"role":{"value":"RootWebArea"},"name":{"value":"Inbox"},"childIds":["2","3"],"backendDOMNodeId":1},
            {"nodeId":"2","ignored":false,"role":{"value":"generic"},"name":{"value":""},"childIds":["4","5"],"backendDOMNodeId":2},
            {"nodeId":"3","ignored":true,"role":{"value":"none"},"childIds":[]},
            {"nodeId":"4","ignored":false,"role":{"value":"button"},"name":{"value":"Compose"},"childIds":[],"backendDOMNodeId":40},
            {"nodeId":"5","ignored":false,"role":{"value":"link"},"name":{"value":"Inbox 3 unread"},"childIds":["6"],"backendDOMNodeId":50},
            {"nodeId":"6","ignored":false,"role":{"value":"StaticText"},"name":{"value":"Inbox"},"childIds":[],"backendDOMNodeId":51}
        ]"#,
        )
        .unwrap()
    }

    #[test]
    fn snapshot_skips_structure_and_shows_refs() {
        let text = format_snapshot(&tree());
        assert_eq!(
            text,
            "- button \"Compose\" [ref=40]\n- link \"Inbox 3 unread\" [ref=50]\n  - text: Inbox\n"
        );
    }

    #[test]
    fn resolves_by_role_name_text_and_ref() {
        let nodes = tree();
        assert_eq!(
            resolve(&nodes, None, Some("Button"), Some("compose"), None)
                .unwrap()
                .0,
            40
        );
        assert_eq!(
            resolve(&nodes, None, Some("link"), Some("inbox"), None)
                .unwrap()
                .0,
            50
        );
        assert_eq!(resolve(&nodes, None, None, None, Some("inbox")).unwrap().0, 50);
        assert_eq!(resolve(&nodes, Some(40), None, None, None).unwrap().2, "Compose");
        assert!(resolve(&nodes, None, Some("button"), Some("send"), None).is_err());
    }

    #[test]
    fn keys() {
        assert_eq!(key_definition("Enter").unwrap().2, 13);
        assert_eq!(key_definition("a").unwrap().3.as_deref(), Some("a"));
        assert!(key_definition("Hyper").is_err());
    }
}
