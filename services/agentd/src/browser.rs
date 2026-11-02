//! Browser tools on the logged-in Chrome, through the DevTools Protocol.
//! Elements are found through the accessibility tree; `ref`s in snapshots
//! are Chrome's backend DOM node IDs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::cdp::{Cdp, Page};
use crate::llm::truncate;
use crate::policy::{Gate, Policy};

/// The element with keyboard focus: its role (explicit, or implied by the
/// tag) and its label.
pub struct Focused {
    pub role: Option<String>,
    pub label: String,
}

/// An element a tool call refers to, found in the accessibility tree.
pub struct Element {
    node: i64,
    pub role: Option<String>,
    pub label: String,
}

pub struct Browser {
    cdp: Cdp,
    page: Option<Page>,
    /// What's happening on the current tab, followed from its events for as
    /// long as agentd is connected to it (across tool calls).
    watch: Mutex<Option<PageWatch>>,
    allow_actions: bool,
    policy: Arc<Policy>,
}

impl Browser {
    pub fn new(cdp: Cdp, allow_actions: bool, policy: Arc<Policy>) -> Self {
        Self {
            cdp,
            page: None,
            watch: Mutex::new(None),
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
        self.connect(&target).await
    }

    async fn connect(&mut self, target: &crate::cdp::Target) -> Result<()> {
        let page = Page::connect(target).await?;
        *self.watch.get_mut() = Some(PageWatch::new(page.events(), page.target_id.clone()));
        self.page = Some(page);
        Ok(())
    }

    pub fn current_target(&self) -> Option<String> {
        self.page.as_ref().map(|p| p.target_id.clone())
    }

    fn page(&self) -> Result<&Page> {
        self.page.as_ref().ok_or_else(|| anyhow!("no tab is open"))
    }

    /// What the current tab did on its own since the last call: dialogs
    /// agentd answered, file pickers it held back (cdp.rs).
    pub fn take_notes(&self) -> Vec<String> {
        self.page.as_ref().map(Page::take_notes).unwrap_or_default()
    }

    async fn switch_to(&mut self, target: &crate::cdp::Target) -> Result<()> {
        self.cdp.activate(&target.id).await?;
        self.connect(target).await
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

    /// Waits until what a navigation or action set off has finished, on
    /// events only, no quiet timers:
    /// 1. a new document (if one is on its way) has fired DOMContentLoaded and
    ///    painted (first contentful paint, or load);
    /// 2. every request of the main frame's own has finished (iframes and
    ///    workers finish in other processes and aren't counted);
    /// 3. two animation frames pass with no element added or removed, no new
    ///    request and no loading indicator. Otherwise, back to 1.
    ///
    /// Measured against the final accessibility tree on Slack, GitHub, Gmail,
    /// Amazon, Notion and Wikipedia, the page is complete at that point, 0.1
    /// to 1.3 s after navigating. `cap` is only a safety net (a hidden tab
    /// doesn't render frames; a request may hang).
    async fn settle(&self, cap: Duration) -> Result<String> {
        let started = Instant::now();
        let deadline = started + cap;
        let took = |how: &str| format!("{how} after {} ms", started.elapsed().as_millis());
        let mut guard = self.watch.lock().await;
        let watch = guard.as_mut().ok_or_else(|| anyhow!("no tab is open"))?;
        loop {
            watch.catch_up();
            if !watch.document_ready() {
                if !watch.next_event(deadline).await {
                    return Ok(took("the new page is still loading; stopped waiting"));
                }
                continue;
            }
            let now = Instant::now();
            if watch.pending(now) > 0 {
                let until = watch.oldest_turns_long(now).min(deadline);
                if now >= deadline {
                    return Ok(took("still loading; stopped waiting"));
                }
                watch.next_event(until).await;
                continue;
            }
            let requests = watch.started;
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.page()?.eval(FRAMES)).await {
                Err(_) => {
                    return Ok(took(
                        "the page isn't rendering (a background tab?); stopped waiting",
                    ));
                }
                // A navigation replaced the document mid-check: the watch saw it.
                Ok(Err(_)) => {}
                Ok(Ok(frames)) => {
                    watch.catch_up();
                    if frames.as_str() == Some("quiet") && watch.started == requests && watch.document_ready()
                    {
                        return Ok(took("ready"));
                    }
                }
            }
            if Instant::now() >= deadline {
                return Ok(took("still changing; stopped waiting"));
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

    /// The wait_for tool: until `text` shows on the page (checked on DOM
    /// changes), or until the page is idle without it (settle), or, without
    /// text, until the page is idle.
    pub async fn wait_for(&self, text: Option<&str>, timeout_secs: u64) -> Result<String> {
        let cap = Duration::from_secs(timeout_secs.clamp(1, 25));
        let outcome = match text.map(str::trim).filter(|t| !t.is_empty()) {
            None => self.settle(cap).await?,
            Some(text) => {
                let needle = serde_json::to_string(&text.to_lowercase())?;
                let script = WAIT_FOR_TEXT
                    .replace("TEXT_JSON", &needle)
                    .replace("MAX_MS", &cap.as_millis().to_string());
                tokio::select! {
                    found = self.eval_across_navigation(&script) => found?,
                    idle = self.settle(cap) => {
                        let idle = idle?;
                        let has = format!("(document.body?.innerText ?? '').toLowerCase().includes({needle})");
                        if self.page()?.eval(&has).await?.as_bool() == Some(true) {
                            format!("found it ({idle})")
                        } else {
                            format!("not on the page, and the page is idle ({idle})")
                        }
                    }
                }
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
        if let Some(watch) = self.watch.get_mut().as_mut() {
            watch.catch_up();
        }
        let result = self.page()?.call("Page.navigate", json!({"url": url})).await?;
        if let Some(error) = result["errorText"].as_str() {
            bail!("couldn't load {url}: {error}");
        }
        // A new document is coming (unless only the #fragment changed).
        if result["loaderId"].as_str().is_some_and(|l| !l.is_empty())
            && let Some(watch) = self.watch.get_mut().as_mut()
        {
            watch.expect_document();
        }
        let settled = self.settle(Duration::from_secs(10)).await?;
        Ok(format!(
            "{} ({settled}){}",
            self.where_am_i().await?,
            self.sign_in_hint().await
        ))
    }

    /// Added to a result when the page asks for a sign-in, so the model hands
    /// over at once instead of spending turns looking at it.
    async fn sign_in_hint(&self) -> &'static str {
        if self.looks_like_sign_in().await {
            ". This is a sign-in page: if the user has to sign in, call needs_user now"
        } else {
            ""
        }
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

    pub fn key_gate(&self, key: &str, focused: &Focused) -> Gate {
        self.policy
            .key_gate(self.allow_actions, key, focused.role.as_deref(), &focused.label)
    }

    pub async fn click(&self, element: &Element) -> Result<String> {
        if let Err(problem) = self.actionable(element, true).await {
            return Ok(format!("not clicked: '{}' {problem}", element.label));
        }
        // A click on "Attach file" would open a system dialog the agent can't see.
        let page = self.page()?;
        page.hold_file_pickers(true).await;
        let clicked = self.click_node(element.node).await;
        page.hold_file_pickers(false).await;
        clicked?;
        let settled = self.settle(Duration::from_secs(8)).await?;
        Ok(format!(
            "clicked '{}' ({settled}); now at {}{}",
            element.label,
            self.where_am_i().await?,
            self.sign_in_hint().await
        ))
    }

    pub async fn type_text(&self, element: &Element, text: &str, submit: bool) -> Result<String> {
        if let Err(problem) = self.actionable(element, false).await {
            return Ok(format!("not typed: '{}' {problem}", element.label));
        }
        let page = self.page()?;
        page.call("DOM.focus", json!({"backendNodeId": element.node}))
            .await?;
        page.call("Input.insertText", json!({"text": text})).await?;
        if submit {
            self.key("Enter").await?;
            self.settle(Duration::from_secs(8)).await?;
        }
        Ok(format!(
            "typed into '{}'; now at {}",
            element.label,
            self.where_am_i().await?
        ))
    }

    pub async fn press_key(&self, key: &str) -> Result<String> {
        // Keys can send, open or scroll in more content (Enter or Space on a
        // file input opens a picker).
        let page = self.page()?;
        page.hold_file_pickers(true).await;
        let pressed = self.key(key).await;
        page.hold_file_pickers(false).await;
        pressed?;
        let settled = self.settle(Duration::from_secs(8)).await?;
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
    pub async fn focused(&self) -> Focused {
        const FOCUSED: &str = "(() => { const e = document.activeElement; if (!e || e === document.body) return null; \
            const label = e.getAttribute('aria-label') || e.getAttribute('placeholder') || e.getAttribute('name') || ''; \
            let role = e.getAttribute('role'); \
            if (!role && e.tagName === 'INPUT') role = e.type === 'search' ? 'searchbox' : e.getAttribute('list') ? 'combobox' : 'textbox'; \
            if (!role && (e.tagName === 'TEXTAREA' || e.isContentEditable)) role = 'textbox'; \
            return {role: role || '', label}; })()";
        let found = match self.page() {
            Ok(page) => page.eval(FOCUSED).await.unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        Focused {
            role: found["role"].as_str().filter(|r| !r.is_empty()).map(String::from),
            label: found["label"].as_str().unwrap_or_default().to_string(),
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

    /// Playwright-style checks before acting on an element, on animation
    /// frames rather than timers: it's attached, visible, in the same place
    /// on two consecutive frames (not animating in), enabled and, for a
    /// click, actually under the pointer (not covered by a banner or dialog).
    /// Err says what's wrong, for the model.
    async fn actionable(&self, element: &Element, hit_test: bool) -> Result<(), String> {
        let Ok(page) = self.page() else { return Ok(()) };
        let _ = page
            .call(
                "DOM.scrollIntoViewIfNeeded",
                json!({"backendNodeId": element.node}),
            )
            .await;
        let Ok(resolved) = page
            .call("DOM.resolveNode", json!({"backendNodeId": element.node}))
            .await
        else {
            return Err("is no longer on the page (take a new snapshot)".into());
        };
        let Some(object) = resolved["object"]["objectId"].as_str() else {
            return Ok(());
        };
        let check = page.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object, "functionDeclaration": ACTIONABLE,
                "arguments": [{"value": hit_test}], "awaitPromise": true, "returnByValue": true,
            }),
        );
        // Still moving after a while (an endless animation): act anyway.
        let verdict = match tokio::time::timeout(Duration::from_secs(2), check).await {
            Ok(Ok(result)) => result["result"]["value"].as_str().unwrap_or("ok").to_string(),
            _ => return Ok(()),
        };
        match verdict.as_str() {
            "ok" => Ok(()),
            "detached" => Err("is no longer on the page (take a new snapshot)".into()),
            "hidden" => Err("isn't visible".into()),
            "disabled" => Err("is disabled".into()),
            covered => Err(format!(
                "is {covered}; deal with that first (close or accept it), then try again"
            )),
        }
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
/// Two animation frames: "quiet" if no element was added or removed (and no
/// busy/hidden flag changed) meanwhile and no loading indicator shows,
/// "changed" otherwise. Text that keeps ticking (presence, "1m ago") doesn't
/// count: apps like Slack never stop that.
const FRAMES: &str = r#"new Promise((resolve) => {
  let changed = false;
  const observer = new MutationObserver((records) => {
    if (records.some((r) => r.type === "attributes" || [...r.addedNodes, ...r.removedNodes].some((n) => n.nodeType === 1))) changed = true;
  });
  observer.observe(document.documentElement, {
    subtree: true, childList: true, attributes: true, attributeFilter: ["aria-busy", "hidden", "aria-hidden"],
  });
  requestAnimationFrame(() => requestAnimationFrame(() => {
    observer.disconnect();
    const busy = !!document.querySelector('[aria-busy="true"], [role="progressbar"]:not([aria-hidden="true"])');
    resolve(changed || busy ? "changed" : "quiet");
  }));
})"#;

/// Run on an element (`this`) before acting on it: resolves "ok", "detached",
/// "hidden", "disabled" or "covered by <what>". Its box must match on two
/// consecutive animation frames; the hit test looks at the element's centre,
/// where the click lands (through shadow roots).
const ACTIONABLE: &str = r#"function (hitTest) {
  const el = this.nodeType === 1 ? this : this.parentElement;
  if (!el || !el.isConnected) return "detached";
  return new Promise((resolve) => {
    let last = "";
    const check = () => {
      if (!el.isConnected) return resolve("detached");
      const r = el.getBoundingClientRect();
      const box = [r.x, r.y, r.width, r.height].join();
      if (box !== last) { last = box; return requestAnimationFrame(check); }
      const style = getComputedStyle(el);
      if (r.width === 0 || r.height === 0 || style.visibility !== "visible") return resolve("hidden");
      if (el.disabled || el.closest('[aria-disabled="true"], fieldset[disabled]')) return resolve("disabled");
      if (hitTest) {
        const x = r.left + r.width / 2, y = r.top + r.height / 2;
        let hit = document.elementFromPoint(x, y);
        while (hit && hit.shadowRoot) {
          const inner = hit.shadowRoot.elementFromPoint(x, y);
          if (!inner || inner === hit) break;
          hit = inner;
        }
        const related = !hit || hit === el || el.contains(hit) || hit.contains(el)
          || (hit.tagName === "LABEL" && hit.control === el)
          || (el.shadowRoot && el.shadowRoot.contains(hit));
        if (!related) {
          const name = (hit.getAttribute("aria-label") || hit.innerText || hit.tagName).trim().replace(/\s+/g, " ").slice(0, 60);
          return resolve(`covered by "${name}"`);
        }
      }
      resolve("ok");
    };
    requestAnimationFrame(check);
  });
}"#;

/// Resolves when TEXT_JSON appears in the page's text (checked when the DOM
/// changes, at most every 150 ms), or after MAX_MS. wait_for also stops as
/// soon as the page is idle without it.
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
      requestAnimationFrame(() => { pending = false; if (has()) finish(`found it after ${Date.now() - started} ms`); });
    });
    observer.observe(document.documentElement, { subtree: true, childList: true, characterData: true });
    const cap = setTimeout(() => finish(`not on the page after ${MAX_MS} ms`), MAX_MS);
  });
})()"#;

/// A request still running after this is a long poll or a stream. A safety
/// valve: requests that matter normally finish well before, and those that
/// never finish here (iframes, workers) aren't counted.
const LONG_REQUEST: Duration = Duration::from_secs(3);
/// Requests a page doesn't wait for: images, fonts and media render on their
/// own; prefetches and pings don't matter; live connections never finish.
const IGNORED_REQUESTS: &[&str] = &[
    "Image",
    "Font",
    "Media",
    "TextTrack",
    "Prefetch",
    "Ping",
    "CSPViolationReport",
    "Manifest",
    "EventSource",
    "WebSocket",
];

/// What's happening on a tab, from its DevTools events: navigations of the
/// main frame, the lifecycle of its document (DOMContentLoaded, first paint,
/// load) and the main frame's own requests. Requests from iframes (cross-site
/// ones run in other processes) and from workers (no loader) aren't counted:
/// this session doesn't see most of them finish.
struct PageWatch {
    events: broadcast::Receiver<Value>,
    /// The main frame's ID (a tab's target ID).
    main_frame: String,
    /// Request ID → when it started, and its document's loader.
    in_flight: HashMap<String, (Instant, String)>,
    /// Requests that matter started so far (to notice new ones).
    started: u64,
    /// A new document is on its way and hasn't painted yet.
    navigating: bool,
    /// The current document's loader, and the lifecycle events it has fired.
    loader: Option<String>,
    lifecycle: Vec<String>,
}

impl PageWatch {
    fn new(events: broadcast::Receiver<Value>, main_frame: String) -> Self {
        Self {
            events,
            main_frame,
            in_flight: HashMap::new(),
            started: 0,
            navigating: false,
            loader: None,
            lifecycle: Vec::new(),
        }
    }

    /// A navigation was asked for: a new document is coming.
    fn expect_document(&mut self) {
        self.navigating = true;
        self.lifecycle.clear();
    }

    fn apply(&mut self, event: &Value) {
        let params = &event["params"];
        let main = |value: &Value| value.as_str() == Some(self.main_frame.as_str());
        match event["method"].as_str() {
            Some("Network.requestWillBeSent") => {
                let loader = params["loaderId"].as_str().unwrap_or_default();
                let kind = params["type"].as_str().unwrap_or("Other");
                if main(&params["frameId"]) && !loader.is_empty() && !IGNORED_REQUESTS.contains(&kind) {
                    if kind == "Document" {
                        self.expect_document();
                    }
                    let id = params["requestId"].as_str().unwrap_or_default().to_string();
                    self.in_flight.insert(id, (Instant::now(), loader.to_string()));
                    self.started += 1;
                }
            }
            Some("Network.loadingFinished" | "Network.loadingFailed") => {
                self.in_flight
                    .remove(params["requestId"].as_str().unwrap_or_default());
            }
            Some("Page.frameRequestedNavigation") => {
                if main(&params["frameId"])
                    && params["disposition"].as_str().is_none_or(|d| d == "currentTab")
                {
                    self.expect_document();
                }
            }
            Some("Page.frameStartedNavigating") => {
                let same_document = matches!(
                    params["navigationType"].as_str(),
                    Some("sameDocument" | "historySameDocument")
                );
                if main(&params["frameId"]) && !same_document {
                    self.expect_document();
                }
            }
            Some("Page.frameNavigated") => {
                let frame = &params["frame"];
                if main(&frame["id"]) && frame["parentId"].is_null() {
                    let loader = frame["loaderId"].as_str().unwrap_or_default().to_string();
                    // The old document's requests are gone with it.
                    self.in_flight.retain(|_, (_, of)| *of == loader);
                    self.loader = Some(loader);
                    self.expect_document();
                }
            }
            Some("Page.lifecycleEvent") => {
                if main(&params["frameId"]) && params["loaderId"].as_str() == self.loader.as_deref() {
                    self.lifecycle
                        .push(params["name"].as_str().unwrap_or_default().to_string());
                    let has = |name: &str| self.lifecycle.iter().any(|seen| seen == name);
                    if has("DOMContentLoaded") && (has("firstContentfulPaint") || has("load")) {
                        self.navigating = false;
                    }
                }
            }
            _ => {}
        }
    }

    /// No new document on its way, or it's parsed and painted.
    fn document_ready(&self) -> bool {
        !self.navigating
    }

    fn catch_up(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(event) => self.apply(&event),
                // Some events were lost: forget requests we may never see end.
                Err(broadcast::error::TryRecvError::Lagged(_)) => self.in_flight.clear(),
                Err(_) => break,
            }
        }
    }

    /// The next event, applied; false once `until` has passed.
    async fn next_event(&mut self, until: Instant) -> bool {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, self.events.recv()).await {
            Ok(Ok(event)) => {
                self.apply(&event);
                true
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                self.in_flight.clear();
                true
            }
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => Instant::now() < until,
        }
    }

    /// Requests that hold the page up: started less than LONG_REQUEST ago.
    fn pending(&self, now: Instant) -> usize {
        self.in_flight
            .values()
            .filter(|(started, _)| now.saturating_duration_since(*started) < LONG_REQUEST)
            .count()
    }

    /// When the oldest pending request stops counting.
    fn oldest_turns_long(&self, now: Instant) -> Instant {
        self.in_flight
            .values()
            .map(|(started, _)| *started + LONG_REQUEST)
            .filter(|at| *at > now)
            .min()
            .unwrap_or(now)
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
    #[test]
    fn only_the_main_frames_own_requests_hold_a_page_up() {
        let (_tx, rx) = broadcast::channel(4);
        let mut watch = PageWatch::new(rx, "MAIN".into());
        let sent = |id: &str, kind: &str, frame: &str, loader: &str| {
            json!({"method": "Network.requestWillBeSent",
                   "params": {"requestId": id, "type": kind, "frameId": frame, "loaderId": loader,
                              "request": {"url": format!("https://x.example/{id}")}}})
        };
        for event in [
            sent("1", "Script", "MAIN", "L1"),
            sent("2", "XHR", "MAIN", "L1"),
            sent("3", "Image", "MAIN", "L1"),      // images don't count
            sent("4", "WebSocket", "MAIN", "L1"),  // nor live connections
            sent("5", "Document", "IFRAME", "L9"), // nor a tracker iframe
            sent("6", "Script", "MAIN", ""),       // nor a worker's script
        ] {
            watch.apply(&event);
        }
        let now = Instant::now();
        assert_eq!(watch.pending(now), 2);
        assert!(watch.document_ready());
        watch.apply(&json!({"method": "Network.loadingFinished", "params": {"requestId": "1"}}));
        assert_eq!(watch.pending(now), 1);
        // A request still running after LONG_REQUEST (a long poll) stops counting.
        assert_eq!(watch.pending(now + LONG_REQUEST), 0);
    }

    #[test]
    fn a_new_document_is_ready_once_parsed_and_painted() {
        let (_tx, rx) = broadcast::channel(4);
        let mut watch = PageWatch::new(rx, "MAIN".into());
        watch.apply(&json!({"method": "Network.requestWillBeSent",
            "params": {"requestId": "old", "type": "XHR", "frameId": "MAIN", "loaderId": "L1", "request": {"url": "u"}}}));
        // A click asks for a navigation: wait for the new document.
        watch.apply(&json!({"method": "Page.frameRequestedNavigation",
            "params": {"frameId": "MAIN", "disposition": "currentTab"}}));
        assert!(!watch.document_ready());
        watch.apply(&json!({"method": "Page.frameNavigated",
            "params": {"frame": {"id": "MAIN", "loaderId": "L2"}}}));
        assert_eq!(watch.pending(Instant::now()), 0); // the old document's request went with it
        let lifecycle = |name: &str, loader: &str| json!({"method": "Page.lifecycleEvent", "params": {"frameId": "MAIN", "loaderId": loader, "name": name}});
        watch.apply(&lifecycle("load", "L1")); // the old document's: ignored
        watch.apply(&lifecycle("DOMContentLoaded", "L2"));
        assert!(!watch.document_ready());
        watch.apply(&lifecycle("firstContentfulPaint", "L2"));
        assert!(watch.document_ready());
        // Same-document navigations (a SPA route) don't wait for a document.
        watch.apply(&json!({"method": "Page.frameStartedNavigating",
            "params": {"frameId": "MAIN", "navigationType": "historySameDocument"}}));
        assert!(watch.document_ready());
    }

    /// Clicks against a real Chrome (CDP_URL): one that navigates (waits for
    /// the new page), one on a button a banner covers, one on a disabled one.
    #[tokio::test]
    #[ignore = "needs a Chrome with remote debugging: CDP_URL=http://127.0.0.1:9222"]
    async fn live_clicks_wait_for_their_effects_and_check_the_target() {
        let cdp = Cdp::new(&std::env::var("CDP_URL").expect("CDP_URL")).unwrap();
        let target = cdp.new_page().await.unwrap();
        let mut browser = Browser::new(cdp.clone(), true, Arc::new(Policy::defaults()));
        browser.switch_to(&target).await.unwrap();

        browser
            .open_url("https://news.ycombinator.com", false)
            .await
            .unwrap();
        let link = browser.find(None, Some("link"), Some("new"), None).await.unwrap();
        let started = Instant::now();
        let clicked = browser.click(&link).await.unwrap();
        println!("{clicked}\n  in {:?}", started.elapsed());
        assert!(clicked.contains("newest"), "{clicked}");

        let page = "data:text/html,<button disabled>Off</button><button>Covered</button>\
            <div style='position:fixed;inset:0;background:rgba(0,0,0,.4)' aria-label='Cookie banner'></div>";
        browser.open_url(page, false).await.unwrap();
        let covered = browser
            .find(None, Some("button"), Some("Covered"), None)
            .await
            .unwrap();
        let result = browser.click(&covered).await.unwrap();
        println!("{result}");
        assert!(result.contains("covered by \"Cookie banner\""), "{result}");
        let off = browser
            .find(None, Some("button"), Some("Off"), None)
            .await
            .unwrap();
        let result = browser.click(&off).await.unwrap();
        println!("{result}");
        assert!(result.contains("disabled"), "{result}");
    }

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
