//! Scheduled tasks: something the user wants done later or on a repeat ("every
//! weekday at 8, summarize my inbox", "tomorrow at 9, check the PR"). agentd
//! starts each run itself, at those times in the user's time zone. A run is an
//! ordinary task (same tools, same approvals) whose answer reaches the chat.
//! The shell's crontab isn't used: it would run commands with no
//! approvals, no model and no way to tell the user.
//!
//! A due run joins a queue in the database (`runs` rows, `queued`), and a
//! background worker takes them one at a time, beside the chat: the user can
//! keep talking to the agent while a check runs. The queue outlives restarts;
//! a run cut off by one, or that failed, is tried once more (agent.rs).

use std::sync::Arc;
use std::time::Duration;

use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned, civil};
use serde_json::json;

use crate::agent::Agent;
use crate::store::Schedule;

/// At most this many schedules.
pub const MAX_SCHEDULES: usize = 25;
/// Repeating schedules run at most this often: each run costs model calls.
pub const MIN_INTERVAL_SECS: i64 = 15 * 60;
/// A run due longer ago than this (agentd was down, or the runs before it in
/// the queue took that long) is skipped rather than run late.
pub const MAX_LATE_SECS: i64 = 3600;
/// One-time runs at most this far ahead.
const MAX_AHEAD_SECS: i64 = 366 * 86_400;

/// What a scheduled run reported with `report_outcome` (docs/memory-v2.md §11).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reported {
    /// result, nothing or blocked.
    pub outcome: String,
    /// At most 180 characters, plain text on one line.
    pub summary: String,
}

/// Cuts text to one plain line: Markdown's marks dropped, at most
/// `max_chars` characters.
pub fn clip(text: &str, max_chars: usize) -> String {
    let flat = plain(text).split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let kept: String = flat.chars().take(max_chars - 1).collect();
    format!("{}…", kept.trim_end())
}

/// Markdown's marks dropped: headings and quotes lose their marks, list
/// items become "•", links their text, and bold, strikethrough and code marks go.
fn plain(text: &str) -> String {
    static BLOCK: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^\s*(?:#{1,6}\s+|>\s?)").expect("valid pattern"));
    static ITEM: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^\s*[-*+]\s+").expect("valid pattern"));
    static LINK: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"!?\[([^\]]*)\]\([^)]*\)").expect("valid pattern"));
    static MARKS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\*\*|__|~~|`").expect("valid pattern"));
    let text = BLOCK.replace_all(text, "");
    let text = ITEM.replace_all(&text, "• ");
    let text = LINK.replace_all(&text, "$1");
    MARKS.replace_all(&text, "").into_owned()
}

/// When a new schedule runs: checked, with its first run.
#[derive(Debug, PartialEq)]
pub struct Timing {
    /// The cron pattern; None for a one-time run.
    pub cron: Option<String>,
    /// Unix seconds.
    pub next_run: i64,
}

/// Standard five-field patterns: minute hour day-of-month month day-of-week.
fn parse(pattern: &str) -> Result<Cron, String> {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(pattern)
        .map_err(|err| {
            format!("bad cron pattern {pattern:?} ({err}); use five fields: minute hour day month weekday")
        })
}

fn zone(timezone: &str) -> Result<TimeZone, String> {
    TimeZone::get(timezone).map_err(|_| format!("unknown time zone {timezone}"))
}

/// Checks a new schedule: a cron pattern (repeating) or a local date and time
/// (once), in the user's time zone.
pub fn plan(cron: Option<&str>, at: Option<&str>, timezone: &str, now: Timestamp) -> Result<Timing, String> {
    let tz = zone(timezone)?;
    let now = now.to_zoned(tz.clone());
    match (given(cron), given(at)) {
        (Some(pattern), None) => {
            let parsed = parse(pattern)?;
            let next = |after: &Zoned| {
                parsed
                    .find_next_occurrence(after, false)
                    .map_err(|err| format!("{pattern:?} never runs ({err})"))
            };
            let first = next(&now)?;
            // Too frequent? Look at the gaps between the next runs.
            let mut previous = first.clone();
            for _ in 0..48 {
                let following = next(&previous)?;
                if following.timestamp().as_second() - previous.timestamp().as_second() < MIN_INTERVAL_SECS {
                    return Err(format!(
                        "{pattern:?} runs more often than every {} minutes, which is the limit",
                        MIN_INTERVAL_SECS / 60
                    ));
                }
                previous = following;
            }
            Ok(Timing {
                cron: Some(pattern.to_string()),
                next_run: first.timestamp().as_second(),
            })
        }
        (None, Some(at)) => {
            let when = local_time(at, &tz)?;
            let ahead = when.timestamp().as_second() - now.timestamp().as_second();
            if ahead < 30 {
                return Err(format!(
                    "{at} is in the past (it's now {})",
                    now.strftime("%Y-%m-%d %H:%M")
                ));
            }
            if ahead > MAX_AHEAD_SECS {
                return Err(format!("{at} is more than a year away"));
            }
            Ok(Timing {
                cron: None,
                next_run: when.timestamp().as_second(),
            })
        }
        _ => Err("give either cron (to repeat) or at (once), not both".into()),
    }
}

fn given(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// "2026-11-02 09:00" (in the user's time zone), or with an offset.
fn local_time(at: &str, tz: &TimeZone) -> Result<Zoned, String> {
    let text = at.trim().replacen(' ', "T", 1);
    if let Ok(instant) = text.parse::<Timestamp>() {
        return Ok(instant.to_zoned(tz.clone()));
    }
    text.parse::<civil::DateTime>()
        .and_then(|local| local.to_zoned(tz.clone()))
        .map_err(|_| format!("bad time {at:?}; use a local date and time like 2026-11-02 09:00"))
}

/// The run after `after`, for a repeating schedule.
pub fn next_run(cron: &str, timezone: &str, after: Timestamp) -> Option<i64> {
    let tz = zone(timezone).ok()?;
    let next = parse(cron)
        .ok()?
        .find_next_occurrence(&after.to_zoned(tz), false)
        .ok()?;
    Some(next.timestamp().as_second())
}

/// "Weekdays at 08:00", "Every 30 minutes", or "Once, Mon Nov 2 at 09:00".
pub fn describe(cron: Option<&str>, next_run: i64, timezone: &str) -> String {
    match cron {
        Some(pattern) => plainly(pattern).unwrap_or_else(|| {
            parse(pattern).map_or_else(
                |_| pattern.to_string(),
                |cron| cron.describe().trim_end_matches('.').to_string(),
            )
        }),
        None => format!("Once, {}", local(next_run, timezone)),
    }
}

/// The common shapes in everyday words; None for the rest.
fn plainly(pattern: &str) -> Option<String> {
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let fields: Vec<&str> = pattern.split_whitespace().collect();
    let [minute, hour, day, month, weekday] = fields[..] else {
        return None;
    };
    if month != "*" {
        return None;
    }
    let every = |field: &str| field.strip_prefix("*/").and_then(|n| n.parse::<u32>().ok());
    if (hour, day, weekday) == ("*", "*", "*")
        && let Some(n) = every(minute)
    {
        return Some(format!("Every {n} minutes"));
    }
    if (minute, day, weekday) == ("0", "*", "*")
        && let Some(n) = every(hour)
    {
        return Some(if n == 1 {
            "Every hour".into()
        } else {
            format!("Every {n} hours")
        });
    }
    // One minute, one or more hours: "07:00 and 19:00".
    let m: u32 = minute.parse().ok()?;
    let hours: Option<Vec<u32>> = hour.split(',').map(|h| h.parse().ok()).collect();
    let times: Vec<String> = hours?.iter().map(|h| format!("{h:02}:{m:02}")).collect();
    let time = match times.as_slice() {
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        [] => return None,
    };
    match (day, weekday) {
        ("*", "*") => Some(format!("Every day at {time}")),
        ("*", "1-5") => Some(format!("Weekdays at {time}")),
        ("*", "0,6" | "6,0" | "6,7" | "SAT,SUN") => Some(format!("Weekends at {time}")),
        ("*", days) => {
            let names: Option<Vec<&str>> = days
                .split(',')
                .map(|d| d.parse::<usize>().ok().map(|d| DAYS[d % 7]))
                .collect();
            let names = names?;
            let list = match names.as_slice() {
                [one] => format!("Every {one}"),
                [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
                [] => return None,
            };
            Some(format!("{list} at {time}"))
        }
        (date, "*") => {
            let date: u32 = date.parse().ok()?;
            let suffix = match (date % 10, date) {
                (_, 11..=13) => "th",
                (1, _) => "st",
                (2, _) => "nd",
                (3, _) => "rd",
                _ => "th",
            };
            Some(format!("Monthly on the {date}{suffix} at {time}"))
        }
        _ => None,
    }
}

/// "Mon Nov 2 at 09:00" in the schedule's time zone.
pub fn local(unix: i64, timezone: &str) -> String {
    let tz = zone(timezone).unwrap_or(TimeZone::UTC);
    Timestamp::from_second(unix)
        .map(|t| t.to_zoned(tz).strftime("%a %b %-d at %H:%M").to_string())
        .unwrap_or_default()
}

/// The schedule for the apps: as stored, plus a description.
pub fn json(schedule: &Schedule) -> serde_json::Value {
    json!({
        "id": schedule.id,
        "name": schedule.name,
        "prompt": schedule.prompt,
        "cron": schedule.cron,
        "timezone": schedule.timezone,
        "description": describe(schedule.cron.as_deref(), schedule.next_run, &schedule.timezone),
        "next_run": schedule.next_run,
        "last_run": schedule.last_run,
        "paused": schedule.paused,
        "created": schedule.created,
        "policy_version": schedule.policy_version,
        "created_from": schedule.created_from,
        "auto_approve": schedule.auto_approve,
    })
}

/// Queues schedules' runs when they're due, for as long as agentd runs.
pub async fn run(agent: Arc<Agent>) {
    loop {
        let now = Timestamp::now().as_second();
        match agent.store.due_schedules(now) {
            Ok(due) => enqueue_due(&agent, due, now),
            Err(err) => tracing::error!(%err, "couldn't read the schedules"),
        }
        // Sleep until the next run is due (at most a minute, in case the clock
        // jumps), or until the schedules change.
        let next = agent.store.next_due().ok().flatten();
        let wait = Duration::from_secs(next.map_or(60, |at| (at - now).clamp(1, 60)) as u64);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = agent.schedules_changed.notified() => {}
        }
    }
}

/// The background worker: runs the queued runs, one at a time and oldest
/// first, beside the chat, for as long as agentd runs.
pub async fn work(agent: Arc<Agent>) {
    loop {
        let now = Timestamp::now().as_second();
        match agent.store.claim_run(now) {
            Ok(Some(queued)) => {
                let schedule = agent.store.schedule(queued.schedule).ok().flatten();
                match schedule {
                    // Paused or cancelled since it was queued.
                    Some(schedule) if schedule.paused => {
                        let _ = agent.store.set_run_status(&queued.id, "cancelled");
                    }
                    None => {
                        let _ = agent.store.set_run_status(&queued.id, "cancelled");
                    }
                    Some(schedule) if now - queued.due > MAX_LATE_SECS => {
                        tracing::warn!(schedule = schedule.id, task = %queued.id, "skipping a queued run that's too late");
                        let _ = agent.store.set_run_status(&queued.id, "skipped");
                        agent.say(&format!(
                            "I skipped the scheduled task “{}” that was due {}: it waited too long behind others.",
                            schedule.name,
                            local(queued.due, &schedule.timezone)
                        ));
                    }
                    Some(schedule) => {
                        tracing::info!(schedule = schedule.id, task = %queued.id, attempt = queued.attempt, late = now - queued.due, "scheduled run started");
                        agent.clone().run_queued(queued, schedule).await;
                    }
                }
                continue;
            }
            Ok(None) => {}
            Err(err) => tracing::error!(%err, "couldn't read the queue"),
        }
        let next = agent.store.next_queued().ok().flatten();
        let wait = Duration::from_secs(next.map_or(60, |at| (at - now).clamp(1, 60)) as u64);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = agent.runs_queued.notified() => {}
        }
    }
}

/// Queues the runs that are due, oldest first, and moves each schedule on.
fn enqueue_due(agent: &Arc<Agent>, due: Vec<Schedule>, now: i64) {
    for schedule in due {
        let late = now - schedule.next_run;
        if late > MAX_LATE_SECS {
            tracing::warn!(schedule = schedule.id, late, "skipping a run that's too late");
            agent.say(&format!(
                "I skipped the scheduled task “{}” that was due {}: the desktop was off or busy then.",
                schedule.name,
                local(schedule.next_run, &schedule.timezone)
            ));
            advance(agent, &schedule, now, None);
            continue;
        }
        // Set up under other rules: it still runs (every action is gated by
        // today's rules anyway), and the chat says so once.
        let current = agent.policy_version();
        if schedule.policy_version.as_deref() != Some(current.as_str()) {
            if schedule.policy_version.is_some() {
                agent.emit(
                    "message",
                    json!({"role": "assistant", "text": format!(
                        "Rules changed since “{}” was set up: it runs under today's rules.", schedule.name
                    )}),
                );
            }
            if let Err(err) = agent.store.set_schedule_policy(schedule.id, &current) {
                tracing::error!(%err, schedule = schedule.id, "couldn't record the schedule's rules");
            }
        }
        let task = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let queued = agent.store.enqueue_run(
            &task,
            schedule.id,
            &schedule.prompt,
            &agent.settings().model,
            &current,
            schedule.next_run,
            0,
        );
        match queued {
            Ok(true) => {
                tracing::info!(schedule = schedule.id, %task, late, "scheduled run queued");
                agent.runs_queued.notify_one();
                advance(agent, &schedule, now, Some(now));
            }
            // Its last run is still queued or running: this one is dropped.
            Ok(false) => {
                tracing::info!(
                    schedule = schedule.id,
                    "a run is still queued or running: not queued again"
                );
                advance(agent, &schedule, now, None);
            }
            Err(err) => tracing::error!(%err, schedule = schedule.id, "couldn't queue a scheduled run"),
        }
    }
}

/// After a run (or a skipped one): a repeating schedule moves on to its next
/// run after now; a one-time one is done.
fn advance(agent: &Agent, schedule: &Schedule, now: i64, ran: Option<i64>) {
    let next = schedule.cron.as_deref().and_then(|cron| {
        let after = Timestamp::from_second(now.max(schedule.next_run)).ok()?;
        next_run(cron, &schedule.timezone, after)
    });
    let result = match next {
        Some(next) => agent.store.schedule_ran(schedule.id, ran, next),
        None => agent.store.delete_schedule(schedule.id).map(|_| ()),
    };
    if let Err(err) = result {
        tracing::error!(%err, schedule = schedule.id, "couldn't update the schedule");
    }
    match next {
        Some(next) => agent.emit("schedule.updated", json!({"id": schedule.id, "next_run": next})),
        None => agent.emit("schedule.deleted", json!({"id": schedule.id, "done": true})),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_long_text() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("one\ntwo   three", 50), "one two three");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn summaries_are_plain_text() {
        assert_eq!(
            clip(
                "## Blogs\n\n**Hacker News:** a new `db`\n- one\n* two\n> quoted [the post](https://x.y/z)",
                180
            ),
            "Blogs Hacker News: a new db • one • two quoted the post"
        );
        // Single marks stay: they're more often maths or names than emphasis.
        assert_eq!(clip("2 * 3 and snake_case", 50), "2 * 3 and snake_case");
    }

    #[tokio::test]
    async fn a_schedule_from_older_rules_runs_and_says_so_once() {
        let (agent, dir) = crate::agent::test_agent();
        let now = Timestamp::now().as_second();
        let schedule = agent
            .store
            .add_schedule(
                "Morning inbox",
                "Summarize my inbox",
                Some("0 8 * * *"),
                "UTC",
                now - 5,
            )
            .unwrap();
        agent
            .store
            .set_schedule_origin(schedule.id, "0000000000000000", "user")
            .unwrap();
        let due = agent.store.due_schedules(now).unwrap();
        enqueue_due(&agent, due, now);
        assert_eq!(agent.store.queued_count().unwrap(), 1); // queued, not started
        let notes: Vec<String> = agent
            .store
            .events_after(0, 100)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "message" && e.data["role"] == "assistant")
            .filter_map(|e| e.data["text"].as_str().map(String::from))
            .collect();
        assert_eq!(
            notes,
            ["Rules changed since “Morning inbox” was set up: it runs under today's rules."]
        );
        let stored = agent.store.schedule(schedule.id).unwrap().unwrap();
        assert_eq!(stored.policy_version, Some(agent.policy_version()));
        assert_eq!(stored.created_from, "user");
        // A schedule from before 0.15 (no version) just takes today's, silently.
        let old = agent
            .store
            .add_schedule("Old", "x", Some("0 9 * * *"), "UTC", now - 5)
            .unwrap();
        enqueue_due(&agent, vec![agent.store.schedule(old.id).unwrap().unwrap()], now);
        assert_eq!(agent.store.queued_count().unwrap(), 2);
        let notes = agent.store.events_after(0, 200).unwrap().into_iter().filter(|e| {
            e.kind == "message"
                && e.data["text"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("Rules changed"))
        });
        assert_eq!(notes.count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    const TORONTO: &str = "America/Toronto";

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn repeating_schedules_run_at_local_times_across_dst() {
        // Saturday 2026-10-31 14:00 UTC = 10:00 in Toronto (EDT, UTC-4).
        let now = at("2026-10-31T14:00:00Z");
        let timing = plan(Some("0 8 * * 1-5"), None, TORONTO, now).unwrap();
        // The next weekday is Monday Nov 2, after DST ended (EST, UTC-5): 13:00 UTC.
        assert_eq!(timing.next_run, at("2026-11-02T13:00:00Z").as_second());
        assert_eq!(timing.cron.as_deref(), Some("0 8 * * 1-5"));
        // And the one after that, Tuesday.
        let next = next_run(
            "0 8 * * 1-5",
            TORONTO,
            Timestamp::from_second(timing.next_run).unwrap(),
        );
        assert_eq!(next, Some(at("2026-11-03T13:00:00Z").as_second()));
        assert_eq!(
            describe(Some("0 8 * * 1-5"), timing.next_run, TORONTO),
            "Weekdays at 08:00"
        );
        for (pattern, words) in [
            ("0 8 * * *", "Every day at 08:00"),
            ("0 7,19 * * *", "Every day at 07:00 and 19:00"),
            ("30 17 * * 5", "Every Friday at 17:30"),
            ("0 9 * * 1,3,5", "Monday, Wednesday and Friday at 09:00"),
            ("0 9 22 * *", "Monthly on the 22nd at 09:00"),
            ("*/30 * * * *", "Every 30 minutes"),
            ("0 */2 * * *", "Every 2 hours"),
        ] {
            assert_eq!(describe(Some(pattern), 0, TORONTO), words);
        }
    }

    #[test]
    fn one_time_runs_are_local_and_in_the_future() {
        let now = at("2026-10-31T14:00:00Z");
        let timing = plan(None, Some("2026-11-02 09:00"), TORONTO, now).unwrap();
        assert_eq!(timing.next_run, at("2026-11-02T14:00:00Z").as_second());
        assert_eq!(timing.cron, None);
        assert_eq!(
            describe(None, timing.next_run, TORONTO),
            "Once, Mon Nov 2 at 09:00"
        );
        // With an explicit offset, too.
        let timing = plan(None, Some("2026-11-02T09:00:00-05:00"), TORONTO, now).unwrap();
        assert_eq!(timing.next_run, at("2026-11-02T14:00:00Z").as_second());
        assert!(plan(None, Some("2026-10-30 09:00"), TORONTO, now).is_err());
        assert!(plan(None, Some("2028-01-01 09:00"), TORONTO, now).is_err());
        assert!(plan(None, Some("tomorrow"), TORONTO, now).is_err());
    }

    #[test]
    fn rejects_bad_or_too_frequent_schedules() {
        let now = at("2026-10-31T14:00:00Z");
        assert!(plan(Some("*/5 * * * *"), None, TORONTO, now).is_err());
        assert!(plan(Some("0,5 8 * * *"), None, TORONTO, now).is_err());
        assert!(plan(Some("*/15 * * * *"), None, TORONTO, now).is_ok());
        assert!(plan(Some("0 8 * *"), None, TORONTO, now).is_err()); // four fields
        assert!(plan(Some("0 0 8 * * *"), None, TORONTO, now).is_err()); // seconds
        assert!(plan(Some("0 25 * * *"), None, TORONTO, now).is_err());
        assert!(plan(Some("0 8 * * *"), Some("2026-11-02 09:00"), TORONTO, now).is_err());
        assert!(plan(None, None, TORONTO, now).is_err());
        assert!(plan(Some("0 8 * * *"), None, "Mars/Olympus", now).is_err());
    }
}
