//! Browser tools on the logged-in Chrome, through the DevTools Protocol.
//! Elements are found through the accessibility tree; `ref`s in snapshots
//! are Chrome's backend DOM node IDs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::cdp::{Cdp, Page};
use crate::llm::truncate;
use crate::policy::{NAVIGATION_KEYS, Policy};

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

    async fn settle(&self) -> Result<()> {
        // Wait for the document, then give client-side apps a moment to render.
        for _ in 0..60 {
            let state = self.page()?.eval("document.readyState").await.unwrap_or_default();
            if state == "complete" || state == "interactive" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
        Ok(())
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
        let result = self.page()?.call("Page.navigate", json!({"url": url})).await?;
        if let Some(error) = result["errorText"].as_str() {
            bail!("couldn't load {url}: {error}");
        }
        self.settle().await?;
        self.where_am_i().await
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

    pub async fn click(
        &self,
        reference: Option<i64>,
        role: Option<&str>,
        name: Option<&str>,
        text: Option<&str>,
    ) -> Result<String> {
        let nodes = self.ax_nodes().await?;
        let (node_id, found_role, label) = resolve(&nodes, reference, role, name, text)?;
        if !self.allow_actions && self.policy.risky_click(found_role.as_deref(), &label) {
            return Ok(format!(
                "blocked: clicking '{label}' is not allowed in read-only mode"
            ));
        }
        self.click_node(node_id).await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        Ok(format!("clicked '{label}'; now at {}", self.where_am_i().await?))
    }

    pub async fn type_text(
        &self,
        reference: Option<i64>,
        role: Option<&str>,
        name: Option<&str>,
        text: &str,
        submit: bool,
    ) -> Result<String> {
        if !self.allow_actions {
            return Ok("blocked: typing in the browser is not allowed in read-only mode".into());
        }
        let nodes = self.ax_nodes().await?;
        let (node_id, _, label) = resolve(&nodes, reference, role, name, None)?;
        let page = self.page()?;
        page.call("DOM.focus", json!({"backendNodeId": node_id})).await?;
        page.call("Input.insertText", json!({"text": text})).await?;
        if submit {
            self.key("Enter").await?;
            self.settle().await?;
        }
        Ok(format!(
            "typed into '{label}'; now at {}",
            self.where_am_i().await?
        ))
    }

    pub async fn press_key(&self, key: &str) -> Result<String> {
        if !self.allow_actions && !NAVIGATION_KEYS.contains(&key) {
            return Ok(format!(
                "blocked: only navigation keys are allowed in read-only mode, not {key}"
            ));
        }
        self.key(key).await?;
        Ok("pressed".into())
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
