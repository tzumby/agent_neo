//! Native apps on the desktop, through Microsoft Research's Hybrid Computer
//! Use (HCU, `computer-use` 1.0.1; docs/desktop-apps.md). agentd (client.rs)
//! asks a broker (broker.rs) that runs as the desktop user, `agent`, and keeps
//! one `computer-use mcp` process (hcu.rs) on the XFCE session's display and
//! bus. This module is what both sides share: the requests and responses on
//! the broker's socket, and reading HCU's text results.
//!
//! HCU reads windows through the accessibility tree (AT-SPI2) and names
//! windows `wN` and elements `rN`. Every input it sends is checked against the
//! live tree first. The broker narrows it to a few typed operations, on the
//! apps the deployment lists, with semantic clicks only.

pub mod broker;
pub mod client;
pub mod hcu;
pub mod steps;

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request on the broker's socket: one line of JSON, one response line back.
/// `task` scopes the refs: a task may use only refs from results it was given
/// since it last took over HCU's observation context (broker.rs).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// The windows of the apps the agent may use.
    Windows {
        task: String,
    },
    /// A window's tree (or `root`'s subtree), or one field of `root` as text.
    Observe {
        task: String,
        window: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        read_text: Option<TextPage>,
    },
    Find {
        task: String,
        window: String,
        query: Query,
    },
    /// What an approval card shows and binds for a batch: per step, the
    /// element (or window) as the app has it now.
    Prepare {
        task: String,
        window: String,
        steps: Vec<steps::Step>,
    },
    /// The batch, if every step's target is still what `expect` describes.
    Act {
        task: String,
        window: String,
        steps: Vec<steps::Step>,
        expect: Vec<Binding>,
    },
    /// The whole desktop: for the user, and the model's frame for pixel input.
    Screenshot {
        task: String,
    },
    /// A closer look at a region of the task's latest screenshot.
    Zoom {
        task: String,
        region: [f64; 4],
    },
    /// What a pixel batch's card binds: the window under each point.
    PrepareGui {
        task: String,
        actions: Vec<steps::GuiAction>,
    },
    Gui {
        task: String,
        actions: Vec<steps::GuiAction>,
        expect: GuiBinding,
    },
}

/// HCU's find filters: at least one; ANDed, case-insensitive.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Query {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_equals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_equals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        [
            &self.role,
            &self.name_equals,
            &self.name_contains,
            &self.text_equals,
            &self.text_contains,
            &self.state,
        ]
        .iter()
        .all(|filter| filter.as_deref().is_none_or(|f| f.trim().is_empty()))
    }
}

/// A page of one element's text (`value`, `name` or `description`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TextPage {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

/// The element an approval is for, as the broker read it from the live app:
/// server-side facts the model's words can't change. The click goes ahead
/// only while all of it still holds.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Binding {
    /// The broker's HCU process: refs die with it.
    pub epoch: String,
    pub window: String,
    pub element: String,
    /// The app as the deployment names it (its display name) and as AT-SPI
    /// does, its process and executable.
    pub app: String,
    pub app_id: String,
    pub pid: u32,
    pub exe: String,
    /// The window's title.
    pub title: String,
    pub role: String,
    pub name: String,
    /// Where it sits: its ancestors, then its description.
    pub path: String,
}

/// Where a pixel batch lands: the screenshot it was planned on (the broker's
/// count of frames), and the window under each of its points and the one
/// that takes its keys. It goes ahead only on this same layout.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct GuiBinding {
    pub epoch: String,
    pub frame: u64,
    pub size: (u32, u32),
    pub targets: Vec<GuiTarget>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct GuiTarget {
    /// Which action (from 0), and where in the screenshot (none for keys and text).
    pub action: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<(f64, f64)>,
    pub app: String,
    pub title: String,
    pub pid: u32,
    pub exe: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Image {
    pub mime: String,
    /// Base64.
    pub data: String,
}

/// The broker's answer. `ok: false` is the broker's own refusal (`error` is
/// one of the codes below); `ok: true` carries HCU's result, which can itself
/// be an error (`hcu_error`), with its evidence line first in `text`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hcu_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Image>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Vec<Binding>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gui: Option<GuiBinding>,
    /// A screenshot's size, in the pixels GUI actions use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Value>,
}

/// The broker's refusals.
pub mod refusal {
    /// HCU, the display or the session isn't there (or the broker isn't).
    pub const UNAVAILABLE: &str = "unavailable";
    /// Not a window of an app the deployment lists.
    pub const NOT_ALLOWED: &str = "not_allowed";
    /// A ref this task wasn't given since it took over the observation
    /// context, or from an HCU process that has since restarted.
    pub const STALE: &str = "stale";
    /// The element isn't what the approval showed any more.
    pub const CHANGED: &str = "changed";
    /// An earlier input's outcome is unknown: someone has to look first.
    pub const INSPECT_FIRST: &str = "inspect_first";
    pub const BUSY: &str = "busy";
    pub const INVALID: &str = "invalid";
}

impl Response {
    pub fn refused(error: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.to_string()),
            message: Some(message.into()),
            ..Self::default()
        }
    }
}

// --- Reading HCU's text -------------------------------------------------------------

/// The evidence line every AX and GUI result starts with: one JSON object
/// (`ok`, `code`, `input_emitted`, `completed`, ...).
pub fn evidence(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text.lines().next()?.trim())
        .ok()
        .filter(Value::is_object)
}

/// Whether input from an act may have had an effect nobody has looked at:
/// anything but a clean, complete success or a refusal before any input.
pub fn input_uncertain(evidence: Option<&Value>) -> bool {
    let Some(e) = evidence else { return true };
    if e["code"] == "effect_unknown" || e["effect_unknown"] == true || e["mutation_unknown"] == true {
        return true;
    }
    if e["ok"] == true {
        return e["completed"] != e["requested"];
    }
    e["input_emitted"] != false
}

/// One window in an AX `apps` listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLine {
    pub window: String,
    pub title: String,
    pub app_id: String,
    pub active: bool,
    /// Listed, but its process isn't on the accessibility bus with a tree.
    pub no_tree: bool,
    /// The line as HCU printed it.
    pub line: String,
}

/// An app HCU names as running but not on the accessibility bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffBus {
    pub name: String,
    pub pid: u32,
    pub line: String,
}

static WINDOW_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(w[1-9][0-9]*) "((?:[^"\\]|\\.)*)"\s+app=(\S+)(.*)$"#).expect("valid regex")
});
static OFF_BUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^Not on the accessibility bus: "((?:[^"\\]|\\.)*)" \(pid ([0-9]+)"#).expect("valid regex")
});

/// HCU quotes names Go-style: `\"` and `\\` inside.
fn unquote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The windows and the apps off the bus in an AX `apps` result.
pub fn parse_apps(text: &str) -> (Vec<WindowLine>, Vec<OffBus>) {
    let mut windows = Vec::new();
    let mut off = Vec::new();
    for line in text.lines() {
        if let Some(c) = WINDOW_LINE.captures(line) {
            let rest = &c[4];
            windows.push(WindowLine {
                window: c[1].to_string(),
                title: unquote(&c[2]),
                app_id: c[3].to_string(),
                active: rest.split("  ").any(|part| part.trim() == "active"),
                no_tree: rest.contains("(no accessibility tree"),
                line: line.to_string(),
            });
        } else if let Some(c) = OFF_BUS.captures(line) {
            off.push(OffBus {
                name: unquote(&c[1]),
                pid: c[2].parse().unwrap_or(0),
                line: line.to_string(),
            });
        }
    }
    (windows, off)
}

static REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([rw][1-9][0-9]*)\b").expect("valid regex"));

/// Every window and element ref a result shows. Generous on purpose (a name
/// that reads "r12" counts too): HCU still checks each ref against the live
/// tree, and the allowlist still holds; this only keeps one task from using
/// refs it was never shown.
pub fn refs(text: &str) -> BTreeSet<String> {
    REF.captures_iter(text).map(|c| c[1].to_string()).collect()
}

/// An element in a tree: its role, its name, and where it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub role: String,
    pub name: String,
    pub path: String,
}

static NODE: LazyLock<Regex> = LazyLock::new(|| {
    // `- <changed> menuitem "Save" [selected=true] [ref=r13]:`
    Regex::new(
        r#"^-\s+(?:<[a-z ]+>\s+)?([^"\[\s:][^"\[:]*?)(?:\s+"((?:[^"\\]|\\.)*)")?(?:\s+\[[^\]]*\])*:?$"#,
    )
    .expect("valid regex")
});

/// A tree line's indent and node (role, name), or None for a field line
/// (`- /description: ...`) or anything else.
fn node(line: &str) -> Option<(usize, String, String)> {
    let indent = line.len() - line.trim_start().len();
    let body = line.trim();
    if body.starts_with("- /") {
        return None;
    }
    let c = NODE.captures(body)?;
    let name = c.get(2).map_or(String::new(), |m| unquote(m.as_str()));
    Some((indent, c[1].trim().to_string(), name))
}

/// Finds `element` (`rN`) in a window tree: its role, its name, and its path
/// (ancestors under the window, then its own description).
pub fn find_element(tree: &str, element: &str) -> Option<Element> {
    let marker = format!("[ref={element}]");
    let lines: Vec<&str> = tree.lines().collect();
    let at = lines.iter().position(|line| line.contains(&marker))?;
    let (indent, role, name) = node(lines[at])?;
    // Its own fields: the lines under it, indented deeper, starting `- /`.
    let description = lines[at + 1..]
        .iter()
        .take_while(|line| line.len() - line.trim_start().len() > indent)
        .find_map(|line| line.trim().strip_prefix("- /description:"))
        .map(|d| unquote(d.trim().trim_matches('"')));
    // Its ancestors: the nearest node above at each smaller indent. The
    // outermost is the window itself when it's a window's role; the card
    // shows the window's title on its own.
    let mut ancestors = Vec::new();
    let mut level = indent;
    for line in lines[..at].iter().rev() {
        if let Some((i, role, name)) = node(line)
            && i < level
        {
            level = i;
            ancestors.push((role, name));
        }
    }
    if ancestors
        .last()
        .is_some_and(|(role, _)| matches!(role.as_str(), "window" | "frame" | "dialog" | "alert"))
    {
        ancestors.pop();
    }
    let mut ancestors: Vec<String> = ancestors
        .iter()
        .rev()
        .map(|(role, name)| described(role, name))
        .collect();
    ancestors.push(described(&role, &name));
    let mut path = ancestors.join(" › ");
    if let Some(description) = description.filter(|d| !d.is_empty()) {
        path.push_str(" — ");
        path.push_str(&description);
    }
    Some(Element { role, name, path })
}

fn described(role: &str, name: &str) -> String {
    if name.is_empty() {
        role.to_string()
    } else {
        format!("{role} “{name}”")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // From the pilot desktop (HCU 1.0.1, Mousepad 0.7, XFCE on Ubuntu 26.04).
    const APPS: &str = r#"{"ok":true,"input_emitted":false}
w1 "~/hcu-pilot/notes.txt - Mousepad"  app=org.xfce.mousepad  active
w2 "Claude - Google Chrome"  app=chrome  (no accessibility tree: use GUI)
w5 "Say \"hi\" - Mousepad"  app=org.xfce.mousepad
Not on the accessibility bus: "xfce4-panel" (pid 15661, GTK). GTK registers on its own unless it was started with NO_AT_BRIDGE=1 or without the session's accessibility bus; restart it from the desktop session. Until then only GUI can read and operate it.
Not on the accessibility bus: "Claude - Google Chrome" (pid 28702, Chromium). Chromium and Electron read the accessibility setting only when they start; quit every window of the application and start it again. Until then only GUI can read and operate it.
"#;

    const MENU_OPEN: &str = r#"{"ok":true,"input_emitted":false}
app: "org.xfce.mousepad"
window: "~/hcu-pilot/notes.txt - Mousepad" [ref=w1]
ax:
 - window "~/hcu-pilot/notes.txt - Mousepad":
  - menu "File" [selected=true] [ref=r1]:
   - /description: File menu
   - menuitem "New" [selected=true] [ref=r8]:
    - /description: Create a new document
   - menuitem "Save" [ref=r13]:
    - /description: Save the current document
   - menuitem "Save As..." [ref=r14]:
    - /description: Save current document as another file
  - menu "Edit" [ref=r2]:
   - /description: Edit menu
  - tablist:
   - textbox [active=true] [ref=r7]:
    - /value: Shopping list - apples - bread
    - /edit-text: "true"
"#;

    #[test]
    fn reads_the_windows_and_the_apps_off_the_bus() {
        let (windows, off) = parse_apps(APPS);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].window, "w1");
        assert_eq!(windows[0].title, "~/hcu-pilot/notes.txt - Mousepad");
        assert_eq!(windows[0].app_id, "org.xfce.mousepad");
        assert!(windows[0].active && !windows[0].no_tree);
        assert!(windows[1].no_tree && !windows[1].active);
        assert_eq!(windows[2].title, "Say \"hi\" - Mousepad");
        assert_eq!(off.len(), 2);
        assert_eq!((off[0].name.as_str(), off[0].pid), ("xfce4-panel", 15661));
        assert_eq!(off[1].pid, 28702);
    }

    #[test]
    fn finds_an_element_with_where_it_sits() {
        let save = find_element(MENU_OPEN, "r13").unwrap();
        assert_eq!(save.role, "menuitem");
        assert_eq!(save.name, "Save");
        assert_eq!(
            save.path,
            "menu “File” › menuitem “Save” — Save the current document"
        );
        let text = find_element(MENU_OPEN, "r7").unwrap();
        assert_eq!((text.role.as_str(), text.name.as_str()), ("textbox", ""));
        assert_eq!(text.path, "tablist › textbox");
        let file = find_element(MENU_OPEN, "r1").unwrap();
        assert_eq!(file.path, "menu “File” — File menu");
        // r1 isn't r13's prefix match: [ref=r1] must close.
        assert!(find_element(MENU_OPEN, "r99").is_none());
    }

    #[test]
    fn reads_a_changed_line_in_a_diff() {
        let diff = " - <changed> menu \"File\" [selected=true] [ref=r1]:\n  - <changed> menuitem \"New Window\" [ref=r9]:\n   - /description: Create a new document in a new window\n";
        let item = find_element(diff, "r9").unwrap();
        assert_eq!(
            (item.role.as_str(), item.name.as_str()),
            ("menuitem", "New Window")
        );
        assert_eq!(
            item.path,
            "menu “File” › menuitem “New Window” — Create a new document in a new window"
        );
    }

    #[test]
    fn collects_refs_and_reads_the_evidence() {
        let refs = refs(MENU_OPEN);
        assert!(refs.contains("w1") && refs.contains("r13") && refs.contains("r7"));
        assert!(!refs.contains("r99"));
        assert_eq!(evidence(APPS).unwrap()["ok"], true);
        assert!(evidence("not json\n").is_none());
    }

    #[test]
    fn knows_when_input_may_have_landed() {
        let line = |s: &str| serde_json::from_str::<Value>(s).ok();
        // Clean and complete.
        assert!(!input_uncertain(
            line(r#"{"ok":true,"input_emitted":true,"completed":2,"requested":2}"#).as_ref()
        ));
        // A refusal before any input.
        assert!(!input_uncertain(
            line(r#"{"ok":false,"code":"stale_ref","input_emitted":false}"#).as_ref()
        ));
        // After input, or not knowing.
        assert!(input_uncertain(
            line(r#"{"ok":false,"code":"wait_timeout","input_emitted":true,"completed":1,"requested":2}"#)
                .as_ref()
        ));
        assert!(input_uncertain(
            line(r#"{"ok":false,"code":"runtime_error"}"#).as_ref()
        ));
        assert!(input_uncertain(
            line(r#"{"ok":true,"input_emitted":true,"completed":1,"requested":2}"#).as_ref()
        ));
        assert!(input_uncertain(
            line(r#"{"ok":false,"code":"effect_unknown","input_emitted":true}"#).as_ref()
        ));
        assert!(input_uncertain(None));
    }

    #[test]
    fn requests_and_queries_round_trip() {
        let request = Request::Find {
            task: "t1".into(),
            window: "w1".into(),
            query: Query {
                role: Some("menu item".into()),
                name_contains: Some("Save".into()),
                ..Query::default()
            },
        };
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(
            line,
            r#"{"op":"find","task":"t1","window":"w1","query":{"role":"menu item","name_contains":"Save"}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        assert!(Query::default().is_empty());
        assert!(
            Query {
                role: Some(" ".into()),
                ..Query::default()
            }
            .is_empty()
        );
    }
}
