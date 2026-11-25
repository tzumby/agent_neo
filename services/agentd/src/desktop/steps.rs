//! What the agent may do in an app (docs/desktop-apps.md §5): the steps of one
//! accessibility batch (`act`) and the pixel actions of one GUI batch, as typed
//! values. They're checked before anything reaches HCU, mapped to HCU's own
//! form, and described for the approval card, which shows every step and the
//! full text of anything typed.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Query;

/// HCU's limits (its contract), and ours where they're tighter.
pub const MAX_STEPS: usize = 64;
/// What fits on a card, as for typing in the browser.
pub const MAX_TEXT: usize = 4000;
/// HCU types non-ASCII one key at a time, at most 500 a call.
pub const MAX_NON_ASCII: usize = 500;
const MAX_WAIT_MS: u64 = 60_000;
const DEFAULT_WAIT_MS: u64 = 5000;
const MAX_KEYS: usize = 8;
const MAX_KEY_LEN: usize = 64;
const MAX_PAGES: f64 = 100.0;
const MAX_SCROLL_DELTA: i64 = 2000;
const MAX_GUI_WAITS: usize = 20;

fn is_false(b: &bool) -> bool {
    !*b
}

/// One step of an accessibility batch.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    /// The element's own action; `pointer` lets HCU fall back to the mouse
    /// (and to raising the window) when the element has none.
    Click {
        #[serde(rename = "ref")]
        element: String,
        #[serde(default, skip_serializing_if = "is_false")]
        pointer: bool,
    },
    /// Text into an editable element (its editable-text interface, or keys).
    Type {
        #[serde(rename = "ref")]
        element: String,
        text: String,
    },
    /// One key chord to the window's focused control, e.g. ["CTRL", "S"].
    Keypress {
        keys: Vec<String>,
    },
    /// An element's whole value (a field's text, a slider, a combo box's choice).
    SetValue {
        #[serde(rename = "ref")]
        element: String,
        value: String,
    },
    /// Selects an element, a substring of it, or a range to `to_ref`.
    Select {
        #[serde(rename = "ref")]
        element: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_ref: Option<String>,
    },
    Scroll {
        #[serde(rename = "ref")]
        element: String,
        direction: String,
        #[serde(default = "one_page")]
        pages: f64,
        #[serde(default, skip_serializing_if = "is_false")]
        pointer: bool,
    },
    /// Switches the batch to another window of an app the agent may use.
    Activate {
        window: String,
    },
    Close {
        window: String,
    },
    /// Sends nothing: waits for a window, an element, a state or text, or for
    /// the UI to settle after the step before.
    Wait(Box<Wait>),
}

fn one_page() -> f64 {
    1.0
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Wait {
    /// A top-level window whose title contains this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<Query>,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub settle: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub gone: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl Step {
    /// The element this step acts on, if any.
    pub fn element(&self) -> Option<&str> {
        match self {
            Step::Click { element, .. }
            | Step::Type { element, .. }
            | Step::SetValue { element, .. }
            | Step::Select { element, .. }
            | Step::Scroll { element, .. } => Some(element),
            _ => None,
        }
    }

    /// Every ref the step names (so the broker can check this task was shown them).
    pub fn refs(&self) -> Vec<&str> {
        match self {
            Step::Select {
                element,
                to_ref: Some(to),
                ..
            } => vec![element, to],
            Step::Activate { window } | Step::Close { window } => vec![window],
            Step::Wait(wait) if wait.element.is_some() => wait.element.iter().map(String::as_str).collect(),
            other => other.element().into_iter().collect(),
        }
    }

    /// Whether it sends input (a wait doesn't).
    pub fn is_input(&self) -> bool {
        !matches!(self, Step::Wait(_))
    }

    /// What the user typed or set: the text the card shows in full.
    pub fn text(&self) -> Option<&str> {
        match self {
            Step::Type { text, .. } => Some(text),
            Step::SetValue { value, .. } => Some(value),
            _ => None,
        }
    }

    /// The step in HCU's form.
    pub fn hcu(&self) -> Value {
        match self {
            Step::Click { element, pointer } => {
                json!({"type": "click", "ref": element, "allow_global_input": pointer})
            }
            Step::Type { element, text } => json!({"type": "type", "ref": element, "text": text}),
            Step::Keypress { keys } => json!({"type": "keypress", "keys": keys}),
            Step::SetValue { element, value } => json!({"type": "set_value", "ref": element, "value": value}),
            Step::Select {
                element,
                text,
                to_ref,
            } => {
                let mut step = json!({"type": "select", "ref": element});
                if let Some(text) = text {
                    step["text"] = json!(text);
                }
                if let Some(to) = to_ref {
                    step["to_ref"] = json!(to);
                }
                step
            }
            Step::Scroll {
                element,
                direction,
                pages,
                pointer,
            } => json!({"type": "scroll", "ref": element, "direction": direction, "pages": pages,
                        "allow_global_input": pointer}),
            Step::Activate { window } => json!({"type": "activate", "ref": window}),
            Step::Close { window } => json!({"type": "close", "ref": window}),
            Step::Wait(wait) => {
                let mut step = serde_json::to_value(wait).unwrap_or_default();
                step["type"] = json!("wait");
                step
            }
        }
    }
}

/// Checks a batch before anything is asked or sent.
pub fn validate(steps: &[Step]) -> Result<(), String> {
    if steps.is_empty() || steps.len() > MAX_STEPS {
        return Err(format!("give 1 to {MAX_STEPS} steps"));
    }
    if !steps.iter().any(Step::is_input) {
        return Err("a batch of only waits sends nothing: use app_observe to look".into());
    }
    let mut waited = 0;
    let mut closed = false;
    for (i, step) in steps.iter().enumerate() {
        let n = i + 1;
        // HCU stops a batch whose window is gone (found live: an activate
        // after a close was refused mid-batch, the close already done).
        if closed && !matches!(step, Step::Wait(_)) {
            return Err(format!(
                "step {n}: a close ends the batch (only waits may follow it); act in the window that's left in a new batch"
            ));
        }
        closed |= matches!(step, Step::Close { .. });
        match step {
            Step::Type { text, .. } | Step::SetValue { value: text, .. } => check_text(text, n)?,
            Step::Keypress { keys } => {
                if keys.is_empty()
                    || keys.len() > MAX_KEYS
                    || keys.iter().any(|k| k.trim().is_empty() || k.len() > MAX_KEY_LEN)
                {
                    return Err(format!(
                        "step {n}: keys is one chord of 1 to {MAX_KEYS} names, like [\"CTRL\", \"S\"]"
                    ));
                }
            }
            Step::Scroll { direction, pages, .. } => {
                if !["up", "down", "left", "right"].contains(&direction.as_str()) {
                    return Err(format!("step {n}: direction is up, down, left or right"));
                }
                if !(*pages > 0.0 && *pages <= MAX_PAGES) {
                    return Err(format!("step {n}: pages is more than 0, at most {MAX_PAGES}"));
                }
            }
            Step::Select { text: Some(text), .. } => check_text(text, n)?,
            Step::Wait(wait) => {
                // HCU's forms: a ref holds only with a value, state or text
                // (found live: {ref: wN, gone} passed here, and HCU refused
                // the batch after its card was approved).
                if let Some(element) = &wait.element
                    && (element.starts_with('w')
                        || (wait.state.is_none() && wait.value.is_none() && wait.text.is_none()))
                {
                    return Err(format!(
                        "step {n}: a ref wait is an element's (rN) state, value or text; to wait for a window to \
                         open or close, give window: its title text (with gone: true to wait for it to close)"
                    ));
                }
                let conditions = [
                    wait.window.is_some(),
                    wait.query.as_ref().is_some_and(|q| !q.is_empty()),
                    wait.element.is_some(),
                    wait.text.is_some() && wait.element.is_none(),
                    wait.settle,
                ];
                if !conditions.iter().any(|c| *c) {
                    return Err(format!(
                        "step {n}: a wait needs a window, a query, a ref (with a state, value or text), text, or settle"
                    ));
                }
                if wait.settle && i == 0 {
                    return Err(format!(
                        "step {n}: settle waits for what the step before it set off"
                    ));
                }
                waited += wait.timeout_ms.unwrap_or(DEFAULT_WAIT_MS);
            }
            _ => {}
        }
    }
    if waited > MAX_WAIT_MS {
        return Err(format!(
            "the waits add up to more than {} seconds",
            MAX_WAIT_MS / 1000
        ));
    }
    Ok(())
}

fn check_text(text: &str, n: usize) -> Result<(), String> {
    if text.is_empty() {
        return Err(format!("step {n}: the text is empty"));
    }
    if text.chars().count() > MAX_TEXT {
        return Err(format!("step {n}: at most {MAX_TEXT} characters a step"));
    }
    if text.chars().filter(|c| !c.is_ascii()).count() > MAX_NON_ASCII {
        return Err(format!(
            "step {n}: at most {MAX_NON_ASCII} non-ASCII characters a step"
        ));
    }
    if text.contains('\0') {
        return Err(format!("step {n}: the text has a NUL"));
    }
    Ok(())
}

/// One pixel action of a GUI batch, in the latest screenshot's pixels.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuiAction {
    Click {
        x: f64,
        y: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keys: Vec<String>,
    },
    DoubleClick {
        x: f64,
        y: f64,
    },
    Move {
        x: f64,
        y: f64,
    },
    Drag {
        path: Vec<Point>,
    },
    Scroll {
        x: f64,
        y: f64,
        #[serde(default)]
        scroll_x: i64,
        #[serde(default)]
        scroll_y: i64,
    },
    Type {
        text: String,
    },
    Keypress {
        keys: Vec<String>,
    },
    /// One second.
    Wait,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl GuiAction {
    /// The pixels it acts at.
    pub fn points(&self) -> Vec<Point> {
        match self {
            GuiAction::Click { x, y, .. }
            | GuiAction::DoubleClick { x, y }
            | GuiAction::Move { x, y }
            | GuiAction::Scroll { x, y, .. } => vec![Point { x: *x, y: *y }],
            GuiAction::Drag { path } => path.clone(),
            _ => Vec::new(),
        }
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            GuiAction::Type { text } => Some(text),
            _ => None,
        }
    }

    pub fn hcu(&self) -> Value {
        let mut action = serde_json::to_value(self).unwrap_or_default();
        if let GuiAction::Wait = self {
            action = json!({"type": "wait"});
        }
        action
    }
}

/// Checks a GUI batch: points inside the frame, sane sizes. Where each point
/// lands is the broker's to check.
pub fn validate_gui(actions: &[GuiAction], frame: (u32, u32)) -> Result<(), String> {
    if actions.is_empty() || actions.len() > MAX_STEPS {
        return Err(format!("give 1 to {MAX_STEPS} actions"));
    }
    if actions.iter().filter(|a| matches!(a, GuiAction::Wait)).count() > MAX_GUI_WAITS {
        return Err(format!("at most {MAX_GUI_WAITS} waits"));
    }
    for (i, action) in actions.iter().enumerate() {
        let n = i + 1;
        for point in action.points() {
            if !(point.x >= 0.0 && point.y >= 0.0 && point.x < frame.0 as f64 && point.y < frame.1 as f64) {
                return Err(format!(
                    "action {n}: ({}, {}) is outside the screenshot ({}×{})",
                    point.x, point.y, frame.0, frame.1
                ));
            }
        }
        match action {
            GuiAction::Drag { path } if path.len() < 2 || path.len() > 256 => {
                return Err(format!("action {n}: a drag's path has 2 to 256 points"));
            }
            GuiAction::Scroll {
                scroll_x, scroll_y, ..
            } if (*scroll_x == 0 && *scroll_y == 0)
                || scroll_x.abs() > MAX_SCROLL_DELTA
                || scroll_y.abs() > MAX_SCROLL_DELTA =>
            {
                return Err(format!(
                    "action {n}: scroll_x or scroll_y, at most {MAX_SCROLL_DELTA}"
                ));
            }
            GuiAction::Type { text } => check_text(text, n)?,
            GuiAction::Keypress { keys }
                if keys.is_empty() || keys.len() > MAX_KEYS || keys.iter().any(|k| k.trim().is_empty()) =>
            {
                return Err(format!("action {n}: keys is one chord of 1 to {MAX_KEYS} names"));
            }
            GuiAction::Click {
                button: Some(button), ..
            } if !["left", "right", "wheel", "back", "forward"].contains(&button.as_str()) => {
                return Err(format!(
                    "action {n}: button is left, right, wheel, back or forward"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(steps: Value) -> Vec<Step> {
        serde_json::from_value(steps).unwrap()
    }

    #[test]
    fn steps_parse_and_map_to_hcus_form() {
        let steps = parse(json!([
            {"type": "click", "ref": "r1"},
            {"type": "wait", "settle": true, "timeout_ms": 3000},
            {"type": "type", "ref": "r7", "text": "Line one\nLíne twö ✓"},
            {"type": "keypress", "keys": ["CTRL", "S"]},
            {"type": "set_value", "ref": "r9", "value": "42"},
            {"type": "select", "ref": "r7", "text": "Line one"},
            {"type": "scroll", "ref": "r7", "direction": "down", "pages": 2},
            {"type": "activate", "window": "w2"},
            {"type": "close", "window": "w3"},
        ]));
        assert!(validate(&steps).is_ok());
        assert_eq!(
            steps[0].hcu(),
            json!({"type": "click", "ref": "r1", "allow_global_input": false})
        );
        assert_eq!(
            steps[1].hcu(),
            json!({"type": "wait", "settle": true, "timeout_ms": 3000})
        );
        assert_eq!(steps[2].hcu()["text"], "Line one\nLíne twö ✓");
        assert_eq!(steps[3].hcu(), json!({"type": "keypress", "keys": ["CTRL", "S"]}));
        assert_eq!(steps[6].hcu()["allow_global_input"], false);
        assert_eq!(steps[7].hcu(), json!({"type": "activate", "ref": "w2"}));
        assert_eq!(steps[5].refs(), vec!["r7"]);
        assert_eq!(steps[8].refs(), vec!["w3"]);
        assert_eq!(steps[2].text(), Some("Line one\nLíne twö ✓"));
    }

    #[test]
    fn bad_batches_are_refused_before_anything_is_sent() {
        let bad = |steps: Value| validate(&parse(steps)).unwrap_err();
        assert!(bad(json!([])).contains("1 to 64"));
        assert!(bad(json!([{"type": "wait", "window": "Save"}])).contains("only waits"));
        assert!(bad(json!([{"type": "type", "ref": "r1", "text": ""}])).contains("empty"));
        assert!(bad(json!([{"type": "type", "ref": "r1", "text": "x".repeat(4001)}])).contains("4000"));
        assert!(bad(json!([{"type": "type", "ref": "r1", "text": "é".repeat(501)}])).contains("non-ASCII"));
        assert!(bad(json!([{"type": "keypress", "keys": []}])).contains("one chord"));
        assert!(bad(json!([{"type": "scroll", "ref": "r1", "direction": "sideways"}])).contains("direction"));
        assert!(
            bad(json!([{"type": "wait", "settle": true}, {"type": "click", "ref": "r1"}])).contains("settle")
        );
        assert!(bad(json!([{"type": "click", "ref": "r1"}, {"type": "wait"}])).contains("a wait needs"));
        let long_waits = json!([{"type": "click", "ref": "r1"}, {"type": "wait", "window": "A", "timeout_ms": 40000},
                                {"type": "wait", "window": "B", "timeout_ms": 30000}]);
        assert!(bad(long_waits).contains("60 seconds"));
        let after_close = json!([{"type": "close", "window": "w3"}, {"type": "wait", "window": "About", "gone": true},
                                 {"type": "activate", "window": "w2"}]);
        assert!(bad(after_close).contains("step 3: a close ends the batch"));
        let window_ref =
            json!([{"type": "click", "ref": "r41"}, {"type": "wait", "ref": "w10", "gone": true}]);
        assert!(bad(window_ref).contains("step 2: a ref wait"));
        let bare_ref = json!([{"type": "click", "ref": "r41"}, {"type": "wait", "ref": "r9"}]);
        assert!(bad(bare_ref).contains("title text"));
        let state =
            json!([{"type": "click", "ref": "r41"}, {"type": "wait", "ref": "r9", "state": "enabled"}]);
        assert!(validate(&parse(state)).is_ok());
        // A type step needs the field it types into.
        assert!(serde_json::from_value::<Vec<Step>>(json!([{"type": "type", "text": "hi"}])).is_err());
    }

    #[test]
    fn gui_actions_stay_inside_the_frame() {
        let actions: Vec<GuiAction> = serde_json::from_value(json!([
            {"type": "click", "x": 30, "y": 60},
            {"type": "drag", "path": [{"x": 10, "y": 10}, {"x": 200, "y": 300}]},
            {"type": "type", "text": "hi"},
            {"type": "keypress", "keys": ["ENTER"]},
            {"type": "wait"},
        ]))
        .unwrap();
        assert!(validate_gui(&actions, (1229, 768)).is_ok());
        assert_eq!(actions[0].hcu(), json!({"type": "click", "x": 30.0, "y": 60.0}));
        assert_eq!(actions[4].hcu(), json!({"type": "wait"}));
        assert_eq!(actions[1].points().len(), 2);
        let outside: Vec<GuiAction> =
            serde_json::from_value(json!([{"type": "click", "x": 1300, "y": 10}])).unwrap();
        assert!(
            validate_gui(&outside, (1229, 768))
                .unwrap_err()
                .contains("outside")
        );
        let still: Vec<GuiAction> =
            serde_json::from_value(json!([{"type": "scroll", "x": 1, "y": 1, "scroll_y": 0}])).unwrap();
        assert!(validate_gui(&still, (1229, 768)).is_err());
    }
}
