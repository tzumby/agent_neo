//! The agent's plan for a task: the steps it means to take, which it keeps
//! current with update_plan. The web app shows it as a checklist while the
//! task runs, and the supervisor (supervise.rs) reads it to judge progress.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_ITEMS: usize = 20;
pub const MAX_STEP_CHARS: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Done,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub step: String,
    pub status: Status,
}

/// update_plan's arguments, as the plan they replace the old one with, or
/// why they can't (for the model).
pub fn parse(args: &Value) -> Result<Vec<Item>, String> {
    let Some(items) = args["items"].as_array() else {
        return Err("items must be a list of {step, status}".into());
    };
    if items.is_empty() {
        return Err("the plan needs at least one step".into());
    }
    if items.len() > MAX_ITEMS {
        return Err(format!("at most {MAX_ITEMS} steps: merge some"));
    }
    items
        .iter()
        .map(|item| {
            let step = item["step"].as_str().map(str::trim).unwrap_or_default();
            if step.is_empty() {
                return Err("every step needs its text".to_string());
            }
            let status = match item["status"].as_str() {
                Some("pending") => Status::Pending,
                Some("in_progress") => Status::InProgress,
                Some("done") => Status::Done,
                _ => return Err("status is pending, in_progress or done".to_string()),
            };
            let step = if step.chars().count() > MAX_STEP_CHARS {
                let cut: String = step.chars().take(MAX_STEP_CHARS - 1).collect();
                format!("{}…", cut.trim_end())
            } else {
                step.to_string()
            };
            Ok(Item { step, status })
        })
        .collect()
}

/// Steps done, and how many there are.
pub fn progress(plan: &[Item]) -> (usize, usize) {
    (
        plan.iter().filter(|i| i.status == Status::Done).count(),
        plan.len(),
    )
}

/// The plan as the supervisor reads it, one step a line.
pub fn describe(plan: &[Item]) -> String {
    plan.iter()
        .map(|item| {
            let mark = match item.status {
                Status::Done => "[done]",
                Status::InProgress => "[in progress]",
                Status::Pending => "[ ]",
            };
            format!("{mark} {}", item.step)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_plan_replaces_the_old_one_whole() {
        let plan = parse(&json!({"items": [
            {"step": "Clone the repo", "status": "done"},
            {"step": " Port the layouts ", "status": "in_progress"},
            {"step": "Deploy", "status": "pending"},
        ]}))
        .unwrap();
        assert_eq!(plan[1].step, "Port the layouts");
        assert_eq!(progress(&plan), (1, 3));
        assert_eq!(
            describe(&plan),
            "[done] Clone the repo\n[in progress] Port the layouts\n[ ] Deploy"
        );
        assert_eq!(
            serde_json::to_value(&plan[1]).unwrap(),
            json!({"step": "Port the layouts", "status": "in_progress"})
        );
    }

    #[test]
    fn bad_plans_say_what_to_fix() {
        assert!(parse(&json!({})).is_err());
        assert!(parse(&json!({"items": []})).is_err());
        assert!(parse(&json!({"items": [{"step": "x", "status": "started"}]})).is_err());
        assert!(parse(&json!({"items": [{"step": "  ", "status": "done"}]})).is_err());
        let many: Vec<Value> = (0..21)
            .map(|i| json!({"step": format!("s{i}"), "status": "pending"}))
            .collect();
        assert!(parse(&json!({ "items": many })).is_err());
        let long = parse(&json!({"items": [{"step": "word ".repeat(60), "status": "pending"}]})).unwrap();
        assert_eq!(long[0].step.chars().count(), MAX_STEP_CHARS);
        assert!(long[0].step.ends_with('…'));
    }
}
