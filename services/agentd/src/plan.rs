//! The agent's plan for a task: the steps it means to take, which it keeps
//! current with update_plan. The apps show it as a checklist while the task
//! runs, and the supervisor (supervise.rs) reads it to judge progress.
//!
//! A step can say what it needs the user's OK for: input on a site or in a
//! desktop app, commands that ask, an install. A plan with needs the task
//! hasn't been granted parks on one `plan` card, which the user can edit; once
//! approved, its needs are the task's grant, and the actions they cover go
//! ahead without their own cards (docs/architecture.md §6.11).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{access, snaps};

pub const MAX_ITEMS: usize = 20;
pub const MAX_STEP_CHARS: usize = 120;
const MAX_APP_CHARS: usize = 60;
const MAX_COMMANDS: usize = 10;
const MAX_COMMAND_CHARS: usize = 2000;

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
    /// The host it clicks, types or presses keys on, in Chrome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// The desktop app it gives input to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Commands it runs that ask, exactly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    /// A Snap Store app it installs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
}

/// What steps need the user's OK for, or what a task was granted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Needs {
    #[serde(default)]
    pub sites: Vec<String>,
    #[serde(default)]
    pub apps: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub installs: Vec<String>,
}

/// A task's grant: the needs of the plans the user approved, the last card
/// that granted them and who answered it (for the record of what it covered).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Grant {
    pub needs: Needs,
    pub approval: Option<String>,
    pub by: Option<String>,
}

/// update_plan's arguments, as the plan they replace the old one with, or
/// why they can't (for the model).
pub fn parse(args: &Value) -> Result<Vec<Item>, String> {
    let Some(items) = args["items"].as_array() else {
        return Err("items must be a list of {step, status}".into());
    };
    items_of(items, true)
}

/// The plan as the user approved it, edited: the same limits as the agent's,
/// statuses optional (a step they add is pending), and no need that wasn't
/// on the card. Errors are for the user.
pub fn edited(card: &Value, plan: &Value) -> Result<Vec<Item>, String> {
    let Some(items) = plan.as_array() else {
        return Err("the plan must be a list of steps".into());
    };
    let items = items_of(items, false)?;
    let asked: Vec<Item> = serde_json::from_value(card["steps"].clone()).unwrap_or_default();
    let asked = Needs::of(&asked);
    let kept = Needs::of(&items);
    if let Some(extra) = kept.without(&asked).first() {
        return Err(format!(
            "a step can only keep what the agent asked for, and {extra} wasn't on the card"
        ));
    }
    Ok(items)
}

fn items_of(items: &[Value], status_required: bool) -> Result<Vec<Item>, String> {
    if items.is_empty() {
        return Err("the plan needs at least one step".into());
    }
    if items.len() > MAX_ITEMS {
        return Err(format!("at most {MAX_ITEMS} steps: merge some"));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let n = i + 1;
            let step = item["step"].as_str().map(str::trim).unwrap_or_default();
            if step.is_empty() {
                return Err("every step needs its text".to_string());
            }
            let status = match item["status"].as_str() {
                Some("pending") => Status::Pending,
                Some("in_progress") => Status::InProgress,
                Some("done") => Status::Done,
                None if !status_required => Status::Pending,
                _ => return Err("status is pending, in_progress or done".to_string()),
            };
            let site = match text(&item["site"]) {
                Some(raw) => Some(access::check_host(raw).map_err(|problem| {
                    format!("step {n}'s site: {problem}; give the host you'll act on, like app.slack.com")
                })?),
                None => None,
            };
            let app = match text(&item["app"]) {
                Some(app) if app.chars().count() > MAX_APP_CHARS || app.chars().any(char::is_control) => {
                    return Err(format!("step {n}'s app: give the app's name, like Mousepad"));
                }
                app => app.map(String::from),
            };
            let commands: Vec<String> = match &item["commands"] {
                Value::Array(list) => list.iter().filter_map(text).map(String::from).collect(),
                one => text(one).map(String::from).into_iter().collect(),
            };
            if commands.len() > MAX_COMMANDS {
                return Err(format!(
                    "step {n}: at most {MAX_COMMANDS} commands; split the step"
                ));
            }
            if commands.iter().any(|c| c.chars().count() > MAX_COMMAND_CHARS) {
                return Err(format!(
                    "step {n}: a command is too long for the plan; it can ask on its own"
                ));
            }
            let install = match text(&item["install"]) {
                Some(name) if !snaps::valid_name(name) => {
                    return Err(format!(
                        "step {n}'s install: give the app's Snap Store name, like slack"
                    ));
                }
                name => name.map(String::from),
            };
            Ok(Item {
                step: clip(step),
                status,
                site,
                app,
                commands,
                install,
            })
        })
        .collect()
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|t| !t.is_empty())
}

fn clip(step: &str) -> String {
    if step.chars().count() > MAX_STEP_CHARS {
        let cut: String = step.chars().take(MAX_STEP_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        step.to_string()
    }
}

/// A command as the plan compares it: each line trimmed, with runs of
/// spaces and tabs as one space. Line breaks still count: they separate
/// commands.
fn normalized(command: &str) -> String {
    command
        .trim()
        .lines()
        .map(|line| {
            line.split([' ', '\t'])
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_new(list: &mut Vec<String>, item: &str) {
    if !list.iter().any(|have| have == item) {
        list.push(item.to_string());
    }
}

impl Needs {
    /// Everything a plan's steps need, each once, in order.
    pub fn of(items: &[Item]) -> Self {
        let mut needs = Self::default();
        for item in items {
            if let Some(site) = &item.site {
                push_new(&mut needs.sites, site);
            }
            if let Some(app) = &item.app
                && !needs.apps.iter().any(|a| a.eq_ignore_ascii_case(app))
            {
                needs.apps.push(app.clone());
            }
            for command in &item.commands {
                if !needs
                    .commands
                    .iter()
                    .any(|c| normalized(c) == normalized(command))
                {
                    needs.commands.push(command.clone());
                }
            }
            if let Some(name) = &item.install {
                push_new(&mut needs.installs, name);
            }
        }
        needs
    }

    pub fn is_empty(&self) -> bool {
        self.sites.is_empty() && self.apps.is_empty() && self.commands.is_empty() && self.installs.is_empty()
    }

    /// What this has that `other` doesn't.
    pub fn without(&self, other: &Self) -> Self {
        Self {
            sites: self
                .sites
                .iter()
                .filter(|s| !other.sites.contains(s))
                .cloned()
                .collect(),
            apps: self
                .apps
                .iter()
                .filter(|a| !other.apps.iter().any(|o| o.eq_ignore_ascii_case(a)))
                .cloned()
                .collect(),
            commands: self
                .commands
                .iter()
                .filter(|c| !other.commands.iter().any(|o| normalized(o) == normalized(c)))
                .cloned()
                .collect(),
            installs: self
                .installs
                .iter()
                .filter(|i| !other.installs.contains(i))
                .cloned()
                .collect(),
        }
    }

    /// Both together, each once.
    pub fn and(&self, other: &Self) -> Self {
        let mut all = self.clone();
        all.sites.extend(other.without(self).sites);
        all.apps.extend(other.without(self).apps);
        all.commands.extend(other.without(self).commands);
        all.installs.extend(other.without(self).installs);
        all
    }

    /// One of them, in words (an edit's refusal names it).
    fn first(&self) -> Option<String> {
        self.sites
            .first()
            .map(|s| format!("input on {s}"))
            .or_else(|| self.apps.first().map(|a| format!("input in {a}")))
            .or_else(|| self.commands.first().map(|c| format!("the command `{c}`")))
            .or_else(|| self.installs.first().map(|i| format!("installing {i}")))
    }

    /// Whether the approved plan covers the action a card would ask about.
    /// Only these kinds: a saved key, a direct connection, a site for the
    /// proxy, a schedule or another plan always gets its own card.
    pub fn covers(&self, card: &Value) -> bool {
        match card["kind"].as_str() {
            // The site and anything under it: app.slack.com for slack.com.
            Some("browser") => card["site"].as_str().is_some_and(|host| {
                self.sites
                    .iter()
                    .any(|site| host == site || host.ends_with(&format!(".{site}")))
            }),
            // Every window it acts in, for pixels too: each step's app.
            Some("app") => {
                let covered = |app: &str| self.apps.iter().any(|a| a.eq_ignore_ascii_case(app.trim()));
                card["app"].as_str().is_some_and(covered)
                    && card["steps"]
                        .as_array()
                        .is_none_or(|steps| steps.iter().filter_map(|s| s["app"].as_str()).all(covered))
            }
            Some("shell") => {
                card["direct"] != true
                    && card["secrets"].as_array().is_none_or(Vec::is_empty)
                    && card["command"].as_str().is_some_and(|command| {
                        self.commands.iter().any(|c| normalized(c) == normalized(command))
                    })
            }
            Some("install") => card["name"]
                .as_str()
                .is_some_and(|name| self.installs.iter().any(|i| i == name)),
            _ => false,
        }
    }

    /// For the model: what goes ahead without asking.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for site in &self.sites {
            parts.push(format!("clicks, typing and keys on {site}"));
        }
        for app in &self.apps {
            parts.push(format!("input in {app}"));
        }
        for command in &self.commands {
            parts.push(format!("the command `{command}`"));
        }
        for name in &self.installs {
            parts.push(format!("installing {name}"));
        }
        parts.join("; ")
    }
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

/// The plan as the user approved it, for the model: numbered.
pub fn numbered(plan: &[Item]) -> String {
    plan.iter()
        .enumerate()
        .map(|(i, item)| format!("{}. {}", i + 1, item.step))
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
        assert!(Needs::of(&plan).is_empty());
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
        // Needs that can't be granted are sent back, not dropped.
        for bad in [
            json!({"site": "com"}),
            json!({"site": "*.slack.com"}),
            json!({"site": "10.0.0.1"}),
            json!({"app": "x".repeat(61)}),
            json!({"install": "Slack Desktop"}),
        ] {
            let mut item = json!({"step": "x", "status": "pending"});
            item.as_object_mut()
                .unwrap()
                .extend(bad.as_object().unwrap().clone());
            assert!(parse(&json!({"items": [item]})).is_err(), "{item}");
        }
    }

    fn needing() -> Vec<Item> {
        parse(&json!({"items": [
            {"step": "Write the list in Mousepad", "status": "pending", "app": "Mousepad"},
            {"step": "Save it", "status": "pending", "app": "mousepad"},
            {"step": "Post it", "status": "pending", "site": "https://app.slack.com/client/T1"},
            {"step": "Push", "status": "pending", "commands": ["git  push -u origin fix", ""]},
            {"step": "Install Slack", "status": "pending", "install": "slack", "site": null},
            {"step": "Tell the user", "status": "pending"},
        ]}))
        .unwrap()
    }

    #[test]
    fn a_plans_needs_are_each_named_once() {
        let items = needing();
        assert_eq!(items[2].site.as_deref(), Some("app.slack.com"));
        assert_eq!(items[3].commands, vec!["git  push -u origin fix"]);
        assert_eq!(
            serde_json::to_value(&items[0]).unwrap(),
            json!({"step": "Write the list in Mousepad", "status": "pending", "app": "Mousepad"})
        );
        let needs = Needs::of(&items);
        assert_eq!(needs.apps, vec!["Mousepad"]);
        assert_eq!(needs.sites, vec!["app.slack.com"]);
        assert_eq!(needs.installs, vec!["slack"]);
        assert_eq!(
            needs.describe(),
            "clicks, typing and keys on app.slack.com; input in Mousepad; the command `git  push -u origin \
             fix`; installing slack"
        );
        // Already granted: nothing new to ask about.
        assert!(needs.without(&needs.clone()).is_empty());
        let mousepad = Needs {
            apps: vec!["MOUSEPAD".into()],
            ..Needs::default()
        };
        assert_eq!(needs.without(&mousepad).apps, Vec::<String>::new());
        assert_eq!(mousepad.and(&needs).apps, vec!["MOUSEPAD"]);
        assert_eq!(mousepad.and(&needs).sites, vec!["app.slack.com"]);
    }

    #[test]
    fn a_grant_covers_only_the_actions_it_names() {
        let grant = Needs::of(&needing());
        let yes = [
            json!({"kind": "browser", "action": "type", "site": "app.slack.com"}),
            json!({"kind": "app", "action": "act", "app": "Mousepad", "steps": [{"action": "type"}]}),
            json!({"kind": "app", "action": "gui", "app": "Mousepad", "steps": [{"app": "Mousepad"}, {"action": "type"}]}),
            json!({"kind": "shell", "command": "git push -u  origin fix", "secrets": []}),
            json!({"kind": "install", "name": "slack"}),
        ];
        for card in yes {
            assert!(grant.covers(&card), "{card}");
        }
        let no = [
            // Another site, or one that only ends the same way.
            json!({"kind": "browser", "action": "click", "site": "slack.com"}),
            json!({"kind": "browser", "action": "click", "site": "evilapp.slack.com.example"}),
            json!({"kind": "browser", "action": "click"}),
            // A pixel batch whose point lands in another app.
            json!({"kind": "app", "action": "gui", "app": "Mousepad", "steps": [{"app": "Slack"}]}),
            json!({"kind": "app", "app": "Slack"}),
            // Another command, one with a saved key, a direct one, one run
            // after another on a new line.
            json!({"kind": "shell", "command": "git push --force -u origin fix"}),
            json!({"kind": "shell", "command": "git push -u origin fix", "secrets": ["GITHUB_TOKEN"]}),
            json!({"kind": "shell", "command": "git push -u origin fix", "direct": true}),
            json!({"kind": "shell", "command": "git push -u origin\nfix"}),
            json!({"kind": "install", "name": "zoom-client"}),
            // Never: keys, sites for the proxy, schedules, plans.
            json!({"kind": "secret", "name": "slack"}),
            json!({"kind": "access", "hosts": ["app.slack.com"]}),
            json!({"kind": "schedule", "name": "slack"}),
            json!({"kind": "plan"}),
        ];
        for card in no {
            assert!(!grant.covers(&card), "{card}");
        }
        // A parent site covers what's under it.
        let slack = Needs {
            sites: vec!["slack.com".into()],
            ..Needs::default()
        };
        assert!(slack.covers(&json!({"kind": "browser", "site": "app.slack.com"})));
        assert!(!slack.covers(&json!({"kind": "browser", "site": "notslack.com"})));
    }

    #[test]
    fn the_user_can_edit_steps_and_drop_needs_but_not_add_them() {
        let card = json!({"kind": "plan", "steps": needing()});
        let plan = edited(
            &card,
            &json!([
                {"step": "Write the list in Mousepad, with eggs", "app": "Mousepad"},
                {"step": "Post it in #random", "status": "pending"},
                {"step": "Push", "status": "pending", "commands": ["git push -u origin fix"]},
            ]),
        )
        .unwrap();
        assert_eq!(plan[0].status, Status::Pending);
        let needs = Needs::of(&plan);
        assert!(needs.sites.is_empty() && needs.installs.is_empty());
        assert_eq!(needs.apps, vec!["Mousepad"]);
        // Nothing the agent didn't ask for.
        for (added, says) in [
            (
                json!({"step": "x", "site": "mail.google.com"}),
                "input on mail.google.com",
            ),
            (
                json!({"step": "x", "commands": ["rm -rf ~"]}),
                "the command `rm -rf ~`",
            ),
            (json!({"step": "x", "app": "Terminal"}), "input in Terminal"),
        ] {
            let problem = edited(&card, &json!([added])).unwrap_err();
            assert!(problem.contains(says), "{problem}");
        }
        assert!(edited(&card, &json!([])).is_err());
        assert!(edited(&card, &json!({"step": "x"})).is_err());
        assert!(edited(&card, &json!([{"step": " "}])).is_err());
    }
}
