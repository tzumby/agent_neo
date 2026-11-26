//! Browser tools on the logged-in Chrome, through the DevTools Protocol.
//! Elements are found through the accessibility tree; `ref`s in snapshots
//! are Chrome's backend DOM node IDs.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::cdp::{Cdp, Page};
use crate::llm::truncate;

// The approval scripts below run in an isolated world (`approval_world`), so
// page JavaScript can't replace the DOM's prototypes there. Named form
// controls and images still shadow properties of a form or the document
// (`<input name="action">` makes `form.action` that input), in every world, so
// these scripts read through the prototypes' own getters and methods.

/// The element a target means: itself, or a text node's parent (across a
/// shadow root's edge). Null for anything else.
const TO_ELEMENT: &str = r#"function() {
    const own = (proto, name) => Object.getOwnPropertyDescriptor(proto, name);
    const get = (proto, name, object) => Reflect.apply(own(proto, name).get, object, []);
    if (get(Node.prototype, 'nodeType', this) === 1) return this;
    const parent = get(Node.prototype, 'parentElement', this);
    if (parent) return parent;
    const root = Reflect.apply(own(Node.prototype, 'getRootNode').value, this, []);
    return root instanceof ShadowRoot ? get(ShadowRoot.prototype, 'host', root) : null;
}"#;

/// The element with keyboard focus, inside open shadow roots too (else the
/// body).
const ACTIVE_ELEMENT: &str = r#"(() => {
    const get = (proto, name, object) =>
        Reflect.apply(Object.getOwnPropertyDescriptor(proto, name).get, object, []);
    let active = get(Document.prototype, 'activeElement', document) || get(Document.prototype, 'body', document);
    for (let root; active && (root = get(Element.prototype, 'shadowRoot', active)); ) {
        const inner = get(ShadowRoot.prototype, 'activeElement', root);
        if (!inner) break;
        active = inner;
    }
    return active;
})()"#;

/// What an approval card shows and binds, with `this` the target element:
/// the page; the text around it (the dialog, row or form it's in, else the
/// nearest parent with more to say than the element itself); where a link or
/// form would go; whether it touches a password or one-time code; and the
/// form's current values. Null when it isn't in this document (detached, or
/// in another frame). Shadow roots count as part of the document.
const INSPECT: &str = r#"function() {
    const own = (proto, name) => Object.getOwnPropertyDescriptor(proto, name);
    const get = (proto, name, object) => Reflect.apply(own(proto, name).get, object, []);
    const call = (proto, name, object, ...args) => Reflect.apply(own(proto, name).value, object, args);
    const attr = (node, name) => call(Element.prototype, 'getAttribute', node, name);
    const rootOf = (node, composed) => call(Node.prototype, 'getRootNode', node, {composed});
    const hostOf = (node) => {
        const root = rootOf(node, false);
        return root instanceof ShadowRoot ? get(ShadowRoot.prototype, 'host', root) : null;
    };
    const parentOf = (node) => get(Node.prototype, 'parentElement', node) || hostOf(node);
    // closest(), continued out through shadow hosts.
    const closest = (node, selector) => {
        for (let at = node; at; at = hostOf(at)) {
            const found = call(Element.prototype, 'closest', at, selector);
            if (found) return found;
        }
        return null;
    };
    const textOf = (node) => node instanceof HTMLElement
        ? get(HTMLElement.prototype, 'innerText', node)
        : get(Node.prototype, 'textContent', node);
    const clean = (text) => String(text || '').replace(/\s+/g, ' ').trim();
    const ofDocument = (name) => get(Document.prototype, name, document);
    const absolute = (value) => {
        try { return new URL(value, get(Node.prototype, 'baseURI', document)).href; }
        catch { return 'about:invalid'; }
    };

    const el = this;
    if (!get(Node.prototype, 'isConnected', el) || rootOf(el, true) !== document) return null;

    let form = el instanceof HTMLFormElement ? el : null;
    if (!form && 'form' in el && el.form instanceof HTMLFormElement) form = el.form;
    if (!form) form = closest(el, 'form');
    const ofForm = (name) => form ? get(HTMLFormElement.prototype, name, form) : '';

    const link = closest(el, 'a[href], area[href]');
    const href = !link ? ''
        : link instanceof HTMLAnchorElement ? get(HTMLAnchorElement.prototype, 'href', link)
        : link instanceof HTMLAreaElement ? get(HTMLAreaElement.prototype, 'href', link)
        : absolute(attr(link, 'href'));
    const submits = (el instanceof HTMLButtonElement || el instanceof HTMLInputElement)
        && call(Element.prototype, 'hasAttribute', el, 'formaction');
    const destination = href || (submits ? el.formAction : ofForm('action')) || '';
    const target = (link && attr(link, 'target')) || attr(el, 'formtarget') || (form && attr(form, 'target')) || '';
    const method = attr(el, 'formmethod') || ofForm('method') || '';

    const sensitive = (field) =>
        (field instanceof HTMLInputElement && get(HTMLInputElement.prototype, 'type', field) === 'password')
        || /(?:^|\s)(?:current-password|new-password|one-time-code)(?:\s|$)/i.test(attr(field, 'autocomplete') || '');
    const controls = form ? Array.from(ofForm('elements')) : [];
    if (el !== form && !controls.includes(el)) controls.push(el);
    const tag = get(Element.prototype, 'localName', el);

    const ownText = clean(textOf(el) || attr(el, 'aria-label'));
    const holder = closest(el,
        'dialog,[role=dialog],[role=alertdialog],tr,[role=row],li,[role=listitem],article,[role=article],form');
    let around = holder ? clean(textOf(holder)) : '';
    if (!around || around === ownText) {
        around = '';
        const body = ofDocument('body');
        for (let at = parentOf(el), i = 0; at && at !== body && i < 6; at = parentOf(at), i++) {
            const text = clean(textOf(at));
            if (text && text !== ownText) { around = text; break; }
        }
    }
    return {
        title: ofDocument('title'), url: location.href,
        around: around.length > 300 ? around.slice(0, 300) + '…' : around,
        label: attr(el, 'aria-label') || attr(el, 'placeholder') || attr(el, 'name') || tag,
        role: attr(el, 'role'), destination, target, method,
        protected: controls.some(sensitive) || ['iframe', 'frame', 'object', 'embed'].includes(tag),
        draft: controls.map((field) => sensitive(field) ? null : [
            field.name, field.value, field.checked,
            field.isContentEditable ? get(Node.prototype, 'textContent', field) : null
        ])
    };
}"#;

/// Whether a click at (x, y) lands on `this`, the reviewed element: what's
/// there is it or inside it (shadow roots included). Text or an icon that
/// takes no pointer events passes the click to the ancestor that does, and a
/// label clicks its control, as they would for a person. Never into a frame.
const HIT_TEST: &str = r#"function(x, y) {
    const own = (proto, name) => Object.getOwnPropertyDescriptor(proto, name);
    const get = (proto, name, object) => Reflect.apply(own(proto, name).get, object, []);
    const call = (proto, name, object, ...args) => Reflect.apply(own(proto, name).value, object, args);
    const hostOf = (node) => {
        const root = call(Node.prototype, 'getRootNode', node);
        return root instanceof ShadowRoot ? get(ShadowRoot.prototype, 'host', root) : null;
    };
    const parentOf = (node) => get(Node.prototype, 'parentElement', node) || hostOf(node);
    const within = (node, container) => {
        for (let at = node; at; at = parentOf(at)) if (at === container) return true;
        return false;
    };
    const el = this;
    // Hit-test in the element's own tree (a closed shadow root included),
    // then down through the open shadow roots under what's there.
    const scope = call(Node.prototype, 'getRootNode', el);
    const proto = scope instanceof ShadowRoot ? ShadowRoot.prototype : Document.prototype;
    let hit = call(proto, 'elementFromPoint', scope, x, y);
    for (let root; hit && (root = get(Element.prototype, 'shadowRoot', hit)); ) {
        const inner = call(ShadowRoot.prototype, 'elementFromPoint', root, x, y);
        if (!inner || inner === hit) break;
        hit = inner;
    }
    if (!hit || ['iframe', 'frame', 'object', 'embed'].includes(get(Element.prototype, 'localName', hit))) {
        return false;
    }
    if (within(hit, el)) return true;
    if (within(el, hit)) {
        for (let at = el; at && at !== hit; at = parentOf(at)) {
            if (getComputedStyle(at).pointerEvents !== 'none') return false;
        }
        return true;
    }
    const labels = 'labels' in el && el.labels instanceof NodeList ? Array.from(el.labels) : [];
    return labels.some((label) => {
        if (!within(hit, label)) return false;
        // Not a link or another control inside the label: those act on their own.
        const control = call(Element.prototype, 'closest', hit,
            'a[href],area[href],button,input,select,textarea,summary,[contenteditable]');
        return !control || control === el || !within(control, label);
    });
}"#;

/// Whether the keyboard focus is on `this`, the reviewed field, or inside it
/// (where a custom element delegates focus), and not in a password or
/// one-time-code field.
const FOCUS_WITHIN: &str = r#"function() {
    const own = (proto, name) => Object.getOwnPropertyDescriptor(proto, name);
    const get = (proto, name, object) => Reflect.apply(own(proto, name).get, object, []);
    const call = (proto, name, object, ...args) => Reflect.apply(own(proto, name).value, object, args);
    const hostOf = (node) => {
        const root = call(Node.prototype, 'getRootNode', node);
        return root instanceof ShadowRoot ? get(ShadowRoot.prototype, 'host', root) : null;
    };
    const parentOf = (node) => get(Node.prototype, 'parentElement', node) || hostOf(node);
    const el = this;
    // The focus as the field's own tree sees it (a closed shadow root
    // included), then down through open shadow roots.
    const scope = call(Node.prototype, 'getRootNode', el);
    let active = get(scope instanceof ShadowRoot ? ShadowRoot.prototype : Document.prototype, 'activeElement', scope);
    for (let root; active && (root = get(Element.prototype, 'shadowRoot', active)); ) {
        const inner = get(ShadowRoot.prototype, 'activeElement', root);
        if (!inner) break;
        active = inner;
    }
    let inside = false;
    for (let at = active; at && !inside; at = parentOf(at)) inside = at === el;
    const sensitive = active instanceof HTMLInputElement
        && get(HTMLInputElement.prototype, 'type', active) === 'password'
        || /(?:^|\s)(?:current-password|new-password|one-time-code)(?:\s|$)/i.test(
            (active && call(Element.prototype, 'getAttribute', active, 'autocomplete')) || '');
    return inside && !sensitive;
}"#;

/// The page and control that the user reviewed.
#[derive(Debug, PartialEq)]
pub struct Place {
    pub title: String,
    /// The page's address without its query or fragment (`shown_url`).
    pub url: String,
    pub around: String,
    pub label: String,
    pub role: Option<String>,
    /// Where a link or form goes, shown like `url`.
    pub destination: Option<String>,
    pub protected: bool,
    /// What a resumed action must still match: the tab, document, element and
    /// form state, and digests of the full page and destination addresses.
    pub binding: Value,
}

/// An element a tool call refers to, found in the accessibility tree.
pub struct Element {
    node: i64,
    pub role: Option<String>,
    pub label: String,
}

/// Where a task's browser works. It connects the first time a tool needs it,
/// so a task that never browses doesn't need Chrome.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Mode {
    /// `main`'s: the remembered tab, else the first one that isn't in
    /// another conversation's window.
    #[default]
    Shared,
    /// Another conversation's: a window of its own, opened the first time
    /// and kept for its next tasks. It sees only that window's tabs.
    Window,
    /// A scheduled run's: a window for this run only, closed at its end
    /// unless it's kept for the user.
    Temporary,
}

pub struct Browser {
    cdp: Cdp,
    page: Option<Page>,
    /// What's happening on the current tab, followed from its events for as
    /// long as agentd is connected to it (across tool calls).
    watch: Mutex<Option<PageWatch>>,
    mode: Mode,
    /// The tab to go back to: `main`'s remembered tab, or a conversation's
    /// window from its earlier tasks.
    remembered: Option<String>,
    /// `main`: a tab of each other conversation's window, which it leaves alone.
    foreign: Vec<String>,
    /// A scheduled run's window, closed when the browser is dropped unless kept.
    opened: Option<String>,
    /// The page as the model last saw it, so an action's result can say
    /// what changed since rather than the model taking a snapshot after it.
    seen: std::sync::Mutex<Option<Seen>>,
}

/// A page's outline (`format_snapshot`) as the model last got it.
struct Seen {
    target: String,
    url: String,
    outline: String,
}

/// After an action on the same page: at most this much of what's new, and
/// of what's gone. After a navigation: at most this much of the new page's
/// outline (snapshot for the rest).
const NEW_MAX_CHARS: usize = 4_000;
const GONE_MAX_CHARS: usize = 1_000;
const OUTLINE_MAX_CHARS: usize = 6_000;

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(id) = self.opened.take() {
            let cdp = self.cdp.clone();
            tokio::spawn(async move {
                if let Err(err) = cdp.close(&id).await {
                    tracing::warn!(%err, "couldn't close a scheduled run's window");
                }
            });
        }
    }
}

impl Browser {
    /// `main`'s browser, with no tab remembered.
    pub fn new(cdp: Cdp) -> Self {
        Self {
            cdp,
            page: None,
            watch: Mutex::new(None),
            mode: Mode::Shared,
            remembered: None,
            foreign: Vec::new(),
            opened: None,
            seen: std::sync::Mutex::new(None),
        }
    }

    /// `main`'s: the tab it used last, never another conversation's window.
    pub fn shared(&mut self, remembered: Option<String>, foreign: Vec<String>) {
        self.mode = Mode::Shared;
        self.remembered = remembered;
        self.foreign = foreign;
    }

    /// Another conversation's: its own window, the one from its earlier
    /// tasks while it's still open.
    pub fn own_window(&mut self, remembered: Option<String>) {
        self.mode = Mode::Window;
        self.remembered = remembered;
    }

    /// A scheduled run's: a window for this run only; the user's tabs are
    /// left alone.
    pub fn temporary_window(&mut self) {
        self.mode = Mode::Temporary;
    }

    /// A scheduled run's browser, which works in one tab of its own.
    pub fn is_temporary(&self) -> bool {
        self.mode == Mode::Temporary
    }

    /// Connects the first time it's needed: to `main`'s tab, the
    /// conversation's window (opened if it's gone), or a new window.
    pub async fn ensure(&mut self) -> Result<()> {
        if self.page.is_some() {
            return Ok(());
        }
        match self.mode {
            Mode::Shared => {
                let pages = self.cdp.pages().await?;
                if let Some(target) = pages.iter().find(|t| Some(&t.id) == self.remembered.as_ref()) {
                    return self.connect(&target.clone()).await;
                }
                // The first tab outside other conversations' windows (tabs()).
                if let Some(target) = self.tabs().await?.into_iter().next() {
                    return self.connect(&target).await;
                }
                let target = self.cdp.new_page().await?;
                if self.in_foreign_window(&target.id).await {
                    // Chrome put it in another conversation's window: a
                    // window of main's own instead.
                    let _ = self.cdp.close(&target.id).await;
                    let target = self.cdp.new_window().await?;
                    return self.connect(&target).await;
                }
                self.connect(&target).await
            }
            Mode::Window => {
                if let Some(id) = &self.remembered
                    && let Some(target) = self.cdp.pages().await?.into_iter().find(|t| &t.id == id)
                {
                    return self.connect(&target).await;
                }
                let target = self.cdp.new_window().await?;
                self.connect(&target).await
            }
            Mode::Temporary => {
                let target = self.cdp.new_window().await?;
                self.opened = Some(target.id.clone());
                self.connect(&target).await
            }
        }
    }

    /// For Action needed: connects to the tab it would use, but never opens
    /// a window just to show it.
    pub async fn ensure_existing(&mut self) {
        if self.page.is_some() {
            return;
        }
        let worth = match self.mode {
            Mode::Shared => true,
            Mode::Window => self.remembered_is_open().await,
            Mode::Temporary => false,
        };
        if worth {
            let _ = self.ensure().await;
        }
    }

    async fn remembered_is_open(&self) -> bool {
        let Some(id) = &self.remembered else { return false };
        self.cdp
            .pages()
            .await
            .is_ok_and(|pages| pages.iter().any(|t| &t.id == id))
    }

    /// A conversation's window comes to the front, for the user to take over.
    pub async fn bring_to_front(&self) {
        if self.mode == Mode::Window
            && let Some(id) = self.current_target()
            && let Err(err) = self.cdp.activate(&id).await
        {
            tracing::warn!(%err, "couldn't bring a conversation's window to the front");
        }
    }

    /// Leaves a scheduled run's window open after all (for the user to take
    /// over); its id.
    pub fn keep_own_window(&mut self) -> Option<String> {
        self.opened.take()
    }

    /// The tabs this task may list and switch to: its window's for a
    /// conversation; for `main`, those outside other conversations' windows.
    async fn tabs(&self) -> Result<Vec<crate::cdp::Target>> {
        let pages = self.cdp.pages().await?;
        let current = self.current_target();
        match self.mode {
            Mode::Shared if self.foreign.is_empty() => Ok(pages),
            Mode::Shared => {
                let ids: Vec<String> = pages.iter().map(|t| t.id.clone()).collect();
                Ok(match self.cdp.windows(&ids).await {
                    Ok(windows) => {
                        let theirs: HashSet<i64> = self
                            .foreign
                            .iter()
                            .filter_map(|t| windows.get(t))
                            .copied()
                            .collect();
                        pages
                            .into_iter()
                            .filter(|t| windows.get(&t.id).is_none_or(|w| !theirs.contains(w)))
                            .collect()
                    }
                    Err(_) => pages
                        .into_iter()
                        .filter(|t| !self.foreign.contains(&t.id))
                        .collect(),
                })
            }
            Mode::Window | Mode::Temporary => {
                let ids: Vec<String> = pages.iter().map(|t| t.id.clone()).collect();
                let mine = match (self.cdp.windows(&ids).await, &current) {
                    (Ok(windows), Some(current)) => windows.get(current).copied().map(|w| (windows, w)),
                    _ => None,
                };
                Ok(match mine {
                    Some((windows, window)) => pages
                        .into_iter()
                        .filter(|t| windows.get(&t.id) == Some(&window))
                        .collect(),
                    None => pages
                        .into_iter()
                        .filter(|t| Some(&t.id) == current.as_ref())
                        .collect(),
                })
            }
        }
    }

    /// Whether `id` landed in another conversation's window (`main` only).
    async fn in_foreign_window(&self, id: &str) -> bool {
        if self.foreign.is_empty() {
            return false;
        }
        let mut ids = self.foreign.clone();
        ids.push(id.to_string());
        let Ok(windows) = self.cdp.windows(&ids).await else {
            return false;
        };
        windows
            .get(id)
            .is_some_and(|window| self.foreign.iter().any(|t| windows.get(t) == Some(window)))
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
        let (title, url) = self.location().await?;
        Ok(format!("{title} | {url}"))
    }

    /// The current tab's title and URL.
    async fn location(&self) -> Result<(String, String)> {
        // `<img name="title">` makes `document.title` that image: read the getter.
        let value = self
            .page()?
            .eval(
                "JSON.stringify([String(Object.getOwnPropertyDescriptor(Document.prototype, 'title')\
                 ?.get?.call(document) ?? ''), location.href])",
            )
            .await?;
        Ok(serde_json::from_str(value.as_str().unwrap_or("[\"\",\"\"]"))?)
    }

    /// The page after an action, for its result: what changed since the
    /// model last saw this page, or, on another page or tab, the new page's
    /// outline. Either way the model has now seen it. Empty when the page
    /// can't be read: the action's own line stands.
    async fn page_after(&self) -> String {
        match self.page_report().await {
            Ok(report) => report,
            Err(err) => {
                tracing::debug!(err = %format!("{err:#}"), "couldn't read the page after an action");
                String::new()
            }
        }
    }

    async fn page_report(&self) -> Result<String> {
        let outline = format_snapshot(&self.ax_nodes().await?);
        let (_, url) = self.location().await?;
        let target = self.current_target().unwrap_or_default();
        let before = self.saw(&target, &url, &outline);
        Ok(match before.filter(|b| b.target == target && b.url == url) {
            Some(before) => describe_changes(&before.outline, &outline),
            None => format!("\n\nThe page:\n{}", truncate(&outline, OUTLINE_MAX_CHARS)),
        })
    }

    /// The model got this page's outline (or what changed in it): returns
    /// what it had seen before.
    fn saw(&self, target: &str, url: &str, outline: &str) -> Option<Seen> {
        self.seen.lock().unwrap().replace(Seen {
            target: target.to_string(),
            url: url.to_string(),
            outline: outline.to_string(),
        })
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
        Ok(format!(
            "{outcome}; now at {}{}",
            self.where_am_i().await?,
            self.page_after().await
        ))
    }

    // --- Tools ---------------------------------------------------------------

    pub async fn list_tabs(&self) -> Result<String> {
        let current = self.current_target();
        let pages = self.tabs().await?;
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
        let pages = self.tabs().await?;
        let target = pages
            .get(index)
            .ok_or_else(|| anyhow!("there is no tab {index}"))?
            .clone();
        self.switch_to(&target).await?;
        Ok(format!(
            "current tab: {}{}",
            self.where_am_i().await?,
            self.page_after().await
        ))
    }

    pub async fn open_url(&mut self, url: &str, new_tab: bool) -> Result<String> {
        // In a window of its own, a conversation or a scheduled run loads
        // everything in its one tab: Chrome puts a new tab in whichever
        // window was last at the front.
        if new_tab && self.mode == Mode::Shared {
            let target = self.cdp.new_page().await?;
            if self.in_foreign_window(&target.id).await {
                // Another conversation's window was at the front: not there.
                let _ = self.cdp.close(&target.id).await;
            } else {
                self.switch_to(&target).await?;
            }
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
            "{} ({settled}){}{}",
            self.where_am_i().await?,
            self.sign_in_hint().await,
            self.page_after().await
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
        let outline = format_snapshot(&self.ax_nodes().await?);
        let (_, url) = self.location().await?;
        self.saw(&self.current_target().unwrap_or_default(), &url, &outline);
        Ok(truncate(&outline, max_chars))
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

    pub async fn click(&self, element: &Element, approved: &Place) -> Result<String> {
        if let Err(problem) = self.actionable(element, true).await {
            return Ok(format!("not clicked: '{}' {problem}", element.label));
        }
        // A click on "Attach file" would open a system dialog the agent can't see.
        let page = self.page()?;
        page.hold_file_pickers(true).await;
        let clicked = self.click_node(element, approved).await;
        page.hold_file_pickers(false).await;
        if let Some(problem) = clicked? {
            return Ok(format!("not clicked: '{}': {problem}", element.label));
        }
        let settled = self.settle(Duration::from_secs(8)).await?;
        Ok(format!(
            "clicked '{}' ({settled}); now at {}{}{}",
            element.label,
            self.where_am_i().await?,
            self.sign_in_hint().await,
            self.page_after().await
        ))
    }

    pub async fn type_text(&self, element: &Element, text: &str, approved: &Place) -> Result<String> {
        if let Err(problem) = self.actionable(element, false).await {
            return Ok(format!("not typed: '{}' {problem}", element.label));
        }
        self.verify_place(Some(element), approved).await?;
        let page = self.page()?;
        page.call("DOM.focus", json!({"backendNodeId": element.node}))
            .await?;
        // Focusing runs the page's handlers: the field must still be as
        // reviewed, and the text goes wherever the focus is now.
        self.verify_place(Some(element), approved)
            .await
            .context("the reviewed field changed")?;
        let (focused, object) = self.run_on_reviewed(approved, FOCUS_WITHIN, json!([])).await?;
        let typed = async {
            anyhow::ensure!(
                focused == true,
                "the reviewed field doesn't have the focus; not typed"
            );
            page.call("Input.insertText", json!({"text": text})).await?;
            anyhow::Ok(())
        }
        .await;
        release(page, &object).await;
        typed?;
        // Suggestions or checks the typing brings up.
        let settled = self.settle(Duration::from_secs(3)).await?;
        Ok(format!(
            "typed into '{}' ({settled}); now at {}{}",
            element.label,
            self.where_am_i().await?,
            self.page_after().await
        ))
    }

    pub async fn press_key(&self, key: &str, approved: &Place) -> Result<String> {
        // Keys can send, open or scroll in more content (Enter or Space on a
        // file input opens a picker).
        let page = self.page()?;
        page.hold_file_pickers(true).await;
        let pressed = async {
            self.verify_place(None, approved).await?;
            self.key(key).await
        }
        .await;
        page.hold_file_pickers(false).await;
        pressed?;
        let settled = self.settle(Duration::from_secs(8)).await?;
        Ok(format!(
            "pressed ({settled}); now at {}{}",
            self.where_am_i().await?,
            self.page_after().await
        ))
    }

    /// Bind approval to the target, main document, node, and current form state.
    /// Evaluate in an isolated world so page JavaScript cannot replace DOM APIs.
    /// A text node stands for its parent element; with no element, the one
    /// with keyboard focus.
    pub async fn place_of(&self, element: Option<&Element>) -> Result<Place> {
        let page = self.page()?;
        let tree = page.call("Page.getFrameTree", json!({})).await?;
        let frame = &tree["frameTree"]["frame"];
        let frame_id = frame["id"].as_str().context("the page has no main frame")?;
        let loader = frame["loaderId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("the page has no stable document")?;
        let context = approval_world(page, frame_id).await?;
        let object = match element {
            Some(element) => element_in(page, context, element.node).await?,
            None => {
                let active = page
                    .call(
                        "Runtime.evaluate",
                        json!({"expression": ACTIVE_ELEMENT, "contextId": context}),
                    )
                    .await?;
                object_of(&active).context("nothing on the page has the focus")?
            }
        };
        let inspected = async {
            let descriptor = page.call("DOM.describeNode", json!({"objectId": object})).await?;
            let backend = descriptor["node"]["backendNodeId"]
                .as_i64()
                .context("the target has no node ID")?;
            let result = page
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object, "functionDeclaration": INSPECT, "returnByValue": true
                    }),
                )
                .await?;
            anyhow::Ok((backend, result))
        }
        .await;
        release(page, &object).await;
        let (backend, result) = inspected?;
        if result.get("exceptionDetails").is_some() {
            tracing::debug!(details = %result["exceptionDetails"], "couldn't inspect a browser target");
            bail!("couldn't inspect the target");
        }
        let value = &result["result"]["value"];
        anyhow::ensure!(value.is_object(), "the target isn't in the reviewed document");
        let field = |name: &str| {
            value[name]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(String::from)
        };
        let url = field("url").context("the page has no URL")?;
        let destination = field("destination");
        Ok(Place {
            title: field("title").unwrap_or_default(),
            url: shown_url(&url),
            around: field("around").unwrap_or_default(),
            label: field("label").unwrap_or_default(),
            role: field("role"),
            destination: destination.as_deref().map(shown_url),
            protected: value["protected"] == true,
            // Digests, not the addresses: a query or fragment can hold a token.
            binding: json!({
                "tab": page.target_id, "frame": frame_id, "document": loader, "node": backend,
                "url": fingerprint(url.as_bytes()),
                "destination": destination.as_deref().map(|d| fingerprint(d.as_bytes())),
                "target": value["target"], "method": value["method"],
                "draft": fingerprint(value["draft"].to_string().as_bytes()),
            }),
        })
    }

    async fn verify_place(&self, element: Option<&Element>, approved: &Place) -> Result<()> {
        let current = self.place_of(element).await?;
        anyhow::ensure!(
            &current == approved && !current.protected,
            "the reviewed browser target changed"
        );
        Ok(())
    }

    /// Runs `function` (with `arguments`) on the reviewed element, the node
    /// `approved` binds (a text node's parent), in a world the page can't
    /// reach. Its value (null if it threw), and the object to release once
    /// done: after acting, so nothing else waits between the check and the act.
    async fn run_on_reviewed(
        &self,
        approved: &Place,
        function: &str,
        arguments: Value,
    ) -> Result<(Value, String)> {
        let page = self.page()?;
        let frame = approved.binding["frame"]
            .as_str()
            .context("the reviewed target has no frame")?;
        let node = approved.binding["node"]
            .as_i64()
            .context("the reviewed target has no node ID")?;
        let context = approval_world(page, frame).await?;
        let object = element_in(page, context, node).await?;
        let result = page
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object, "functionDeclaration": function,
                    "arguments": arguments, "returnByValue": true,
                }),
            )
            .await;
        match result {
            Ok(result) if result.get("exceptionDetails").is_some() => Ok((Value::Null, object)),
            Ok(result) => Ok((result["result"]["value"].clone(), object)),
            Err(err) => {
                release(page, &object).await;
                Err(err)
            }
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

    /// Clicks the reviewed element at its centre. Some(why not) when something
    /// else is at that spot by then: nothing is clicked.
    async fn click_node(&self, element: &Element, approved: &Place) -> Result<Option<String>> {
        let page = self.page()?;
        let backend_id = element.node;
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
        self.verify_place(Some(element), approved).await?;
        // The click goes to whatever is at (x, y), so check that it's the
        // element the user reviewed, one round trip before the click.
        let (hit, object) = self
            .run_on_reviewed(approved, HIT_TEST, json!([{"value": x}, {"value": y}]))
            .await?;
        let on_target = hit == true;
        let clicked = async {
            if on_target {
                for (kind, buttons) in [("mousePressed", 1), ("mouseReleased", 0)] {
                    page.call(
                        "Input.dispatchMouseEvent",
                        json!({"type": kind, "x": x, "y": y, "button": "left", "buttons": buttons, "clickCount": 1}),
                    )
                    .await?;
                }
            }
            anyhow::Ok(())
        }
        .await;
        release(page, &object).await;
        clicked?;
        Ok((!on_target).then(|| {
            "the page changed under the target (something else is at that spot now). Take a new \
             snapshot and ask again."
                .to_string()
        }))
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

/// A new isolated world on the frame: its execution context. Page scripts
/// can't reach it or change the DOM prototypes it sees.
async fn approval_world(page: &Page, frame: &str) -> Result<i64> {
    let world = page
        .call(
            "Page.createIsolatedWorld",
            json!({"frameId": frame, "worldName": "agentd-approval"}),
        )
        .await?;
    world["executionContextId"]
        .as_i64()
        .context("could not inspect the page")
}

/// The element `node` means (a text node means its parent), as an object in
/// `context`. Release it when done.
async fn element_in(page: &Page, context: i64, node: i64) -> Result<String> {
    let resolved = page
        .call(
            "DOM.resolveNode",
            json!({"backendNodeId": node, "executionContextId": context}),
        )
        .await?;
    let object = resolved["object"]["objectId"]
        .as_str()
        .context("the target is no longer on the page")?
        .to_string();
    let element = page
        .call(
            "Runtime.callFunctionOn",
            json!({"objectId": object, "functionDeclaration": TO_ELEMENT}),
        )
        .await;
    release(page, &object).await;
    object_of(&element?).context("the target isn't an element on the page")
}

/// The object a script returned; None for a primitive, null, or an error it
/// threw.
fn object_of(result: &Value) -> Option<String> {
    if result.get("exceptionDetails").is_some() {
        return None;
    }
    result["result"]["objectId"].as_str().map(String::from)
}

async fn release(page: &Page, object: &str) {
    let _ = page
        .call("Runtime.releaseObject", json!({"objectId": object}))
        .await;
}

/// SHA-256, base64url: what a binding keeps of text it shouldn't hold.
fn fingerprint(bytes: &[u8]) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// An address as a card shows it: scheme, host, port and path. No user name
/// or password, query or fragment, which can hold tokens. Only the scheme
/// for an address without a host (`javascript:`, `data:`, `about:`).
fn shown_url(raw: &str) -> String {
    match reqwest::Url::parse(raw) {
        Ok(url) if url.cannot_be_a_base() => format!("{}:", url.scheme()),
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => "about:invalid".into(),
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

/// What changed between two outlines of a page, line by line (a line is an
/// element or a text, whatever its depth): what's new, in page order with
/// its refs, and what's gone. A line that only moved is unchanged.
pub fn describe_changes(before: &str, after: &str) -> String {
    let mut left: HashMap<&str, usize> = HashMap::new();
    for line in before.lines() {
        *left.entry(line.trim()).or_default() += 1;
    }
    let mut new = Vec::new();
    for line in after.lines() {
        match left.get_mut(line.trim()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => new.push(line),
        }
    }
    let mut gone = Vec::new();
    for line in before.lines() {
        if let Some(count) = left.get_mut(line.trim())
            && *count > 0
        {
            *count -= 1;
            gone.push(line.trim());
        }
    }
    if new.is_empty() && gone.is_empty() {
        return "\n\nThe page didn't change.".into();
    }
    let mut out = String::new();
    if !new.is_empty() {
        // As deep as the shallowest new line: the new part's own structure.
        let indent = new
            .iter()
            .map(|line| line.len() - line.trim_start_matches(' ').len())
            .min()
            .unwrap_or(0);
        let lines: Vec<&str> = new.iter().map(|line| &line[indent..]).collect();
        out.push_str(&format!(
            "\n\nNew on the page:\n{}",
            truncate(&lines.join("\n"), NEW_MAX_CHARS)
        ));
    }
    if !gone.is_empty() {
        out.push_str(&format!(
            "\n\nGone from the page:\n{}",
            truncate(&gone.join("\n"), GONE_MAX_CHARS)
        ));
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
        let mut browser = Browser::new(cdp.clone());
        browser.switch_to(&target).await.unwrap();

        browser
            .open_url("https://news.ycombinator.com", false)
            .await
            .unwrap();
        let link = browser.find(None, Some("link"), Some("new"), None).await.unwrap();
        let started = Instant::now();
        let clicked = browser
            .click(&link, &browser.place_of(Some(&link)).await.unwrap())
            .await
            .unwrap();
        println!("{clicked}\n  in {:?}", started.elapsed());
        assert!(clicked.contains("newest"), "{clicked}");

        let page = "data:text/html,<button disabled>Off</button><button>Covered</button>\
            <div style='position:fixed;inset:0;background:rgba(0,0,0,.4)' aria-label='Cookie banner'></div>";
        browser.open_url(page, false).await.unwrap();
        let covered = browser
            .find(None, Some("button"), Some("Covered"), None)
            .await
            .unwrap();
        let result = browser
            .click(&covered, &browser.place_of(Some(&covered)).await.unwrap())
            .await
            .unwrap();
        println!("{result}");
        assert!(result.contains("covered by \"Cookie banner\""), "{result}");
        let off = browser
            .find(None, Some("button"), Some("Off"), None)
            .await
            .unwrap();
        let result = browser
            .click(&off, &browser.place_of(Some(&off)).await.unwrap())
            .await
            .unwrap();
        println!("{result}");
        assert!(result.contains("disabled"), "{result}");
    }

    #[tokio::test]
    #[ignore = "needs CDP_URL pointing at a Chrome"]
    async fn live_actions_say_what_changed_on_the_page() {
        let cdp = Cdp::new(&std::env::var("CDP_URL").expect("CDP_URL")).unwrap();
        let target = cdp.new_page().await.unwrap();
        let mut browser = Browser::new(cdp.clone());
        browser.switch_to(&target).await.unwrap();
        let page = "data:text/html,<title>Mail</title><button>Compose</button>\
            <button onclick=\"this.remove()\">Dismiss</button><div id=d></div><script>\
            document.querySelector('button').onclick = () => { document.getElementById('d').innerHTML = \
            '<div role=dialog aria-label=\"New message\"><input aria-label=\"To\"></div>'; };</script>";
        let opened = browser.open_url(page, false).await.unwrap();
        println!("{opened}\n---");
        assert!(
            opened.contains("The page:\n") && opened.contains("button \"Compose\" [ref="),
            "{opened}"
        );

        let compose = browser
            .find(None, Some("button"), Some("Compose"), None)
            .await
            .unwrap();
        let place = browser.place_of(Some(&compose)).await.unwrap();
        let clicked = browser.click(&compose, &place).await.unwrap();
        println!("{clicked}\n---");
        assert!(
            clicked.contains("New on the page:\n- dialog \"New message\" [ref="),
            "{clicked}"
        );
        assert!(clicked.contains("textbox \"To\" [ref="), "{clicked}");
        assert!(!clicked.contains("Gone from the page"), "{clicked}");

        // Typing changes nothing else here.
        let to = browser
            .find(None, Some("textbox"), Some("To"), None)
            .await
            .unwrap();
        let place = browser.place_of(Some(&to)).await.unwrap();
        let typed = browser.type_text(&to, "ana@example.com", &place).await.unwrap();
        println!("{typed}\n---");
        assert!(typed.contains("typed into 'To'"), "{typed}");

        let dismiss = browser
            .find(None, Some("button"), Some("Dismiss"), None)
            .await
            .unwrap();
        let place = browser.place_of(Some(&dismiss)).await.unwrap();
        let gone = browser.click(&dismiss, &place).await.unwrap();
        println!("{gone}\n---");
        assert!(
            gone.contains("Gone from the page:\n- button \"Dismiss\""),
            "{gone}"
        );
        let _ = cdp.close(&target.id).await;
    }

    #[tokio::test]
    #[ignore = "needs a local Chrome with remote debugging"]
    async fn live_browser_approval_follows_the_control_and_form_state() {
        let cdp = Cdp::new(&std::env::var("CDP_URL").expect("CDP_URL")).unwrap();
        let target = cdp.new_page().await.unwrap();
        let mut browser = Browser::new(cdp.clone());
        browser.switch_to(&target).await.unwrap();
        browser.open_url(
            "data:text/html,<form><input aria-label='Message' value='original'><button type='button'>Save</button></form>",
            false,
        ).await.unwrap();

        let button = browser
            .find(None, Some("button"), Some("Save"), None)
            .await
            .unwrap();
        let approved = browser.place_of(Some(&button)).await.unwrap();
        browser
            .page()
            .unwrap()
            .eval("document.querySelector('input').value = 'new draft'")
            .await
            .unwrap();
        assert!(browser.verify_place(Some(&button), &approved).await.is_err());
        let field = browser
            .find(None, Some("textbox"), Some("Message"), None)
            .await
            .unwrap();
        let approved = browser.place_of(Some(&field)).await.unwrap();
        browser.type_text(&field, " typed", &approved).await.unwrap();
        let value = browser
            .page()
            .unwrap()
            .eval("document.querySelector('input').value")
            .await
            .unwrap();
        assert!(value.as_str().unwrap().contains(" typed"));
        cdp.close(&target.id).await.unwrap();
    }

    /// Inspecting an approval target and taking a screenshot leave the DOM intact.
    #[tokio::test]
    #[ignore = "needs CDP_URL pointing at a Chrome"]
    async fn live_approval_inspection_leaves_the_page_as_it_was() {
        let cdp = Cdp::new(&std::env::var("CDP_URL").expect("CDP_URL")).unwrap();
        let target = cdp.new_page().await.unwrap();
        let mut browser = Browser::new(cdp.clone());
        browser.switch_to(&target).await.unwrap();
        let page = "data:text/html,<title>Invoices</title>\
            <table><tr><td>March from Acme, $120</td><td><button>Plain</button></td></tr></table>\
            <button style='color: red'>Styled</button><button style=''>Empty</button>";
        browser.open_url(page, false).await.unwrap();
        for (name, expected) in [
            ("Plain", Value::Null),
            ("Styled", json!("color: red")),
            ("Empty", json!("")),
        ] {
            let element = browser
                .find(None, Some("button"), Some(name), None)
                .await
                .unwrap();
            browser.place_of(Some(&element)).await.unwrap();
            let picture = browser.screenshot_jpeg().await.unwrap();
            assert!(!picture.is_empty());
            let style = browser
                .page()
                .unwrap()
                .eval(&format!(
                    "[...document.querySelectorAll('button')].find(b => b.textContent === '{name}').getAttribute('style')"
                ))
                .await
                .unwrap();
            assert_eq!(style, expected, "{name}");
        }
        let plain = browser
            .find(None, Some("button"), Some("Plain"), None)
            .await
            .unwrap();
        let place = browser.place_of(Some(&plain)).await.unwrap();
        assert_eq!(place.title, "Invoices");
        assert!(place.around.contains("March from Acme, $120"), "{}", place.around);
    }

    /// The page these approval tests run on, served from localhost so it has
    /// an http address with a query and a fragment.
    const TARGETS_PAGE: &str = r#"<!doctype html><title>Orders</title><img name="title">
        <form action="https://collect.example/submit?token=abc" method="post">
          <input type="hidden" name="action" value="update">
          <input type="hidden" name="method" value="get">
          <input name="elements" aria-label="Elements"><input type="password" name="pin">
          <button type="button">Pay</button>
        </form>
        <div onclick="this.dataset.clicked = 1"><span>Archive this</span></div>
        <div style="position: relative; width: 160px; height: 30px">
          <input type="checkbox" id="news" aria-label="News"
            style="position: absolute; left: 0; top: 0; width: 20px; height: 20px; margin: 0">
          <label for="news" style="position: absolute; inset: 0">Newsletter</label>
        </div>
        <my-field></my-field>
        <ul><li>Shipping label <button id="real" onclick="window.realClicked = 1">Real</button></li></ul>
        <p><button id="decoy" onclick="window.decoyClicked = 1">Decoy</button></p>
        <script>
          customElements.define('my-field', class extends HTMLElement {
            connectedCallback() {
              this.attachShadow({mode: 'open'}).innerHTML =
                '<form action="/save?draft=1"><input aria-label="Nickname"></form>';
            }
          });
        </script>"#;

    async fn serve(html: &'static str) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                    html.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        address
    }

    /// Approval targets against a real Chrome (CDP_URL): named controls can't
    /// hide where a form goes, how, or that it has a password; plain text and a
    /// shadow root's field are targets; cards and bindings leave out the
    /// query; a label clicks its control; and a click doesn't land on what the
    /// page put over the target after the review, even when the page fools
    /// the main world's own hit test.
    #[tokio::test]
    #[ignore = "needs CDP_URL pointing at a Chrome"]
    async fn live_approval_targets_hold_against_the_page() {
        let address = serve(TARGETS_PAGE).await;
        let cdp = Cdp::new(&std::env::var("CDP_URL").expect("CDP_URL")).unwrap();
        let target = cdp.new_page().await.unwrap();
        let mut browser = Browser::new(cdp.clone());
        browser.switch_to(&target).await.unwrap();
        browser
            .open_url(&format!("http://{address}/orders?session=s3cret#top"), false)
            .await
            .unwrap();
        let eval = |expression: &'static str| {
            let page = browser.page().unwrap();
            async move { page.eval(expression).await.unwrap() }
        };
        // The page's own world does see the named controls in place of the properties.
        assert_eq!(
            eval("[typeof document.title, typeof document.forms[0].action, typeof document.forms[0].method]")
                .await,
            json!(["object", "object", "object"])
        );

        let pay = browser
            .find(None, Some("button"), Some("Pay"), None)
            .await
            .unwrap();
        let place = browser.place_of(Some(&pay)).await.unwrap();
        assert_eq!(place.title, "Orders");
        assert_eq!(place.url, format!("http://{address}/orders"));
        assert_eq!(
            place.destination.as_deref(),
            Some("https://collect.example/submit")
        );
        assert!(place.protected, "the form has a password field");
        assert_eq!(place.binding["method"], "post");
        assert_eq!(
            place.binding["destination"],
            fingerprint(b"https://collect.example/submit?token=abc")
        );
        let binding = place.binding.to_string();
        assert!(
            !binding.contains("s3cret") && !binding.contains("token=abc"),
            "{binding}"
        );

        // Plain text: its element is the target, and the click reaches the handler.
        let text = browser
            .find(None, None, None, Some("Archive this"))
            .await
            .unwrap();
        let place = browser.place_of(Some(&text)).await.unwrap();
        assert_eq!(place.label, "span");
        let clicked = browser.click(&text, &place).await.unwrap();
        assert!(clicked.starts_with("clicked"), "{clicked}");
        assert_eq!(eval("document.querySelector('div').dataset.clicked").await, "1");

        // A label over its checkbox: the click goes through it.
        let news = browser
            .find(None, Some("checkbox"), Some("News"), None)
            .await
            .unwrap();
        let clicked = browser
            .click(&news, &browser.place_of(Some(&news)).await.unwrap())
            .await
            .unwrap();
        assert!(clicked.starts_with("clicked"), "{clicked}");
        assert_eq!(eval("document.getElementById('news').checked").await, true);

        // A field in a shadow root: reviewed, typed into, and what has the focus.
        let field = browser
            .find(None, Some("textbox"), Some("Nickname"), None)
            .await
            .unwrap();
        let place = browser.place_of(Some(&field)).await.unwrap();
        assert_eq!(place.label, "Nickname");
        assert_eq!(place.destination, Some(format!("http://{address}/save")));
        let typed = browser.type_text(&field, "Neo", &place).await.unwrap();
        assert!(typed.starts_with("typed"), "{typed}");
        assert_eq!(
            eval("document.querySelector('my-field').shadowRoot.querySelector('input').value").await,
            "Neo"
        );
        assert_eq!(browser.place_of(None).await.unwrap().label, "Nickname");

        // After the review, the page puts another button over the reviewed one
        // and fools the page's own hit test: nothing is clicked.
        let real = browser
            .find(None, Some("button"), Some("Real"), None)
            .await
            .unwrap();
        let place = browser.place_of(Some(&real)).await.unwrap();
        eval(
            "(() => {
                const real = document.getElementById('real'), r = real.getBoundingClientRect();
                Object.assign(document.getElementById('decoy').style, {position: 'fixed', zIndex: 10,
                    left: r.left + 'px', top: r.top + 'px', width: r.width + 'px', height: r.height + 'px'});
                document.elementFromPoint = () => real;
            })()",
        )
        .await;
        let clicked = browser.click(&real, &place).await.unwrap();
        assert!(clicked.contains("the page changed under the target"), "{clicked}");
        assert_eq!(
            eval("[window.realClicked ?? null, window.decoyClicked ?? null]").await,
            json!([null, null])
        );
        cdp.close(&target.id).await.unwrap();
    }

    #[test]
    fn cards_show_addresses_without_their_query_and_bindings_keep_digests() {
        assert_eq!(
            shown_url("https://me:pw@mail.example.com:8443/inbox/42?token=abc#message"),
            "https://mail.example.com:8443/inbox/42"
        );
        assert_eq!(shown_url("https://example.com"), "https://example.com/");
        assert_eq!(shown_url("javascript:alert(document.cookie)"), "javascript:");
        assert_eq!(shown_url("data:text/html,<b>private</b>"), "data:");
        assert_eq!(shown_url("not an address"), "about:invalid");
        // What agentd checks on a card works on what the card shows.
        assert!(crate::guard::check_url(&shown_url("https://example.com/a?b=c")).is_ok());
        assert!(crate::guard::check_url(&shown_url("http://127.0.0.1:9222/json?x=1")).is_err());
        assert!(crate::guard::check_url(&shown_url("javascript:void(0)")).is_err());
        let digest = fingerprint(b"https://example.com/?token=abc");
        assert_ne!(digest, fingerprint(b"https://example.com/?token=abd"));
        assert!(!digest.contains("token"));
    }

    #[test]
    fn only_an_object_a_script_returned_is_an_element() {
        assert_eq!(
            object_of(&json!({"result": {"type": "object", "objectId": "1"}})),
            Some("1".into())
        );
        assert_eq!(
            object_of(&json!({"result": {"type": "object", "subtype": "null", "value": null}})),
            None
        );
        assert_eq!(
            object_of(&json!({
                "result": {"type": "object", "subtype": "error", "objectId": "2"},
                "exceptionDetails": {"text": "Uncaught"}
            })),
            None
        );
    }

    #[tokio::test]
    #[ignore = "needs CDP_URL pointing at a Chrome"]
    async fn live_open_url_waits_for_the_page_to_settle() {
        let cdp = Cdp::new(&std::env::var("CDP_URL").expect("CDP_URL")).unwrap();
        let target = cdp.new_page().await.unwrap();
        println!("tab {}", target.id);
        let mut browser = Browser::new(cdp.clone());
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
    fn an_action_says_what_changed_on_the_page() {
        let before = "- main\n  - button \"Compose\" [ref=40]\n  - link \"Inbox 3 unread\" [ref=50]\n";
        // A dialog opened deep in the tree; the inbox count changed.
        let after = "- main\n  - button \"Compose\" [ref=40]\n  - link \"Inbox 2 unread\" [ref=51]\n  \
                     - dialog \"New message\" [ref=60]\n    - textbox \"To\" [ref=61]\n";
        assert_eq!(
            describe_changes(before, after),
            "\n\nNew on the page:\n- link \"Inbox 2 unread\" [ref=51]\n- dialog \"New message\" [ref=60]\n  \
             - textbox \"To\" [ref=61]\n\nGone from the page:\n- link \"Inbox 3 unread\" [ref=50]"
        );
        assert_eq!(describe_changes(before, before), "\n\nThe page didn't change.");
        // Moved, not changed: the same lines at another depth.
        let moved = "- button \"Compose\" [ref=40]\n- main\n    - link \"Inbox 3 unread\" [ref=50]\n";
        assert_eq!(describe_changes(before, moved), "\n\nThe page didn't change.");
        // Two equal lines and one goes: one is gone, not both.
        let twice = "- text: Saved\n- text: Saved\n";
        assert_eq!(
            describe_changes(twice, "- text: Saved\n"),
            "\n\nGone from the page:\n- text: Saved"
        );
        // A big change is cut short.
        let big: String = (0..2000).map(|i| format!("- text: row {i}\n")).collect();
        let report = describe_changes("", &big);
        assert!(
            report.len() < NEW_MAX_CHARS + 200 && report.contains("[truncated:"),
            "{}",
            report.len()
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
