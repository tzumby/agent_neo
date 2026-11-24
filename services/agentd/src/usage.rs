//! What the model costs, and the monthly budget. Every model call's tokens
//! are priced with llm.rs's table and written to its run's row as they
//! happen (agent.rs `record_usage`), so a running task counts. Days and
//! months are the user's (`user_timezone`; UTC when unknown).
//!
//! The budget (`monthly_budget_usd`) alerts once a month at 80% and at
//! 100%; once it's used up, new tasks wait in the queue, scheduled runs are
//! skipped and memory tidy-up waits, until it's raised, removed, or the month
//! turns over. Tasks already running finish, and an answered card resumes.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};
use serde_json::{Value, json};

use crate::agent::{Agent, Settings};
use crate::notify::Notice;
use crate::store::Store;

/// The largest monthly budget the settings take, in USD.
pub const MAX_BUDGET_USD: f64 = 100_000.0;
/// `by_session` and `by_schedule` list at most this many.
const TOP: usize = 10;

/// The monthly budget in USD, if one is set.
pub fn monthly_budget(store: &Store) -> Option<f64> {
    store
        .setting("monthly_budget_usd")
        .ok()
        .flatten()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
}

/// The user's time zone for the figures: its name and the zone (UTC when
/// unknown).
fn zone(store: &Store) -> (String, TimeZone) {
    store
        .setting("user_timezone")
        .ok()
        .flatten()
        .and_then(|name| TimeZone::get(&name).ok().map(|tz| (name, tz)))
        .unwrap_or_else(|| ("UTC".into(), TimeZone::UTC))
}

/// How `runs.started` is written, for comparing: "2026-11-01T04:00:00.000Z".
fn iso(at: Timestamp) -> String {
    at.strftime("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

/// The start of a local day in `tz`.
fn start_of(day: Date, tz: &TimeZone) -> Result<Timestamp> {
    Ok(day.to_zoned(tz.clone())?.timestamp())
}

/// Dollars kept to a millionth, so sums read cleanly.
fn round(usd: f64) -> f64 {
    (usd * 1e6).round() / 1e6
}

/// This month's spending against the budget.
pub struct Budget {
    pub monthly_usd: Option<f64>,
    pub spent: f64,
    /// `YYYY-MM`, the user's.
    pub month: String,
}

impl Budget {
    pub fn fraction(&self) -> Option<f64> {
        self.monthly_usd.map(|budget| self.spent / budget)
    }

    /// Used up: new tasks wait.
    pub fn paused(&self) -> bool {
        self.monthly_usd.is_some_and(|budget| self.spent >= budget)
    }

    /// For `/status` and `/usage`.
    pub fn json(&self) -> Value {
        json!({
            "monthly_usd": self.monthly_usd, "spent": round(self.spent),
            "fraction": self.fraction().map(|f| (f * 1e4).round() / 1e4),
            "paused": self.paused(), "month": self.month,
        })
    }
}

impl Agent {
    /// This month's spending (every run, running ones included) and budget.
    pub fn budget(&self) -> Budget {
        self.budget_at(Timestamp::now())
    }

    /// `budget`, as of `now`.
    fn budget_at(&self, now: Timestamp) -> Budget {
        let (_, tz) = zone(&self.store);
        let today = now.to_zoned(tz.clone()).date();
        let start = start_of(today.first_of_month(), &tz).unwrap_or(now);
        Budget {
            monthly_usd: monthly_budget(&self.store),
            spent: self.store.cost_since(&iso(start)).unwrap_or(0.0),
            month: today.strftime("%Y-%m").to_string(),
        }
    }

    /// The budget is used up: new tasks wait in the queue.
    pub fn budget_paused(&self) -> bool {
        monthly_budget(&self.store).is_some() && self.budget().paused()
    }

    /// Sets the monthly budget (None removes it). Err says what's wrong
    /// with the value. Raising or removing it lets the queue go again.
    pub fn set_budget(self: &Arc<Self>, usd: Option<f64>) -> Result<Result<Settings, String>> {
        match usd {
            Some(usd) if !(usd.is_finite() && usd > 0.0 && usd <= MAX_BUDGET_USD) => {
                return Ok(Err(format!(
                    "monthly_budget_usd must be a positive number up to {MAX_BUDGET_USD}"
                )));
            }
            Some(usd) => self.store.set_setting("monthly_budget_usd", &usd.to_string())?,
            None => self.store.delete_setting("monthly_budget_usd")?,
        }
        tracing::info!(budget = ?usd, "monthly budget changed");
        let settings = self.settings();
        self.emit("settings", json!(settings));
        self.check_budget();
        self.pump();
        Ok(Ok(settings))
    }

    /// Alerts at 80% and 100% of the monthly budget, each once a month
    /// (`budget_alerted`): a `budget.alert` event and a notification.
    pub fn check_budget(&self) {
        self.check_budget_at(Timestamp::now());
    }

    /// `check_budget`, as of `now`.
    fn check_budget_at(&self, now: Timestamp) {
        if monthly_budget(&self.store).is_none() {
            return;
        }
        let _one_at_a_time = self.budget_check.lock().unwrap();
        let budget = self.budget_at(now);
        let Some(limit) = budget.monthly_usd else {
            return;
        };
        let this_month = format!("{}:", budget.month);
        let mut alerted: Vec<String> = self
            .store
            .setting("budget_alerted")
            .ok()
            .flatten()
            .unwrap_or_default()
            .split(',')
            .filter(|mark| mark.starts_with(&this_month))
            .map(String::from)
            .collect();
        for (level, reached) in [(80, budget.spent >= limit * 0.8), (100, budget.spent >= limit)] {
            let mark = format!("{this_month}{level}");
            if !reached || alerted.contains(&mark) {
                continue;
            }
            alerted.push(mark);
            if let Err(err) = self.store.set_setting("budget_alerted", &alerted.join(",")) {
                tracing::error!(%err, "couldn't record a budget alert");
                return;
            }
            tracing::warn!(level, spent = budget.spent, budget = limit, "budget alert");
            self.emit(
                "budget.alert",
                json!({"level": level, "spent": round(budget.spent), "budget": limit, "month": budget.month}),
            );
            let amount = if limit.fract() == 0.0 {
                format!("${limit:.0}")
            } else {
                format!("${limit:.2}")
            };
            let (title, body, urgency) = if level == 100 {
                (
                    "Monthly budget used up".to_string(),
                    format!(
                        "You've used all of this month's {amount} budget. New messages wait until you raise it."
                    ),
                    "high",
                )
            } else {
                (
                    "80% of the monthly budget".to_string(),
                    format!("You've used 80% of this month's {amount} budget"),
                    "normal",
                )
            };
            let notice = Notice {
                title,
                body,
                tag: format!("budget-{level}"),
                urgency,
                ttl: 86_400,
                data: json!({"budget": level}),
                session: None,
            };
            if let Err(err) = self.notify(&notice, None) {
                tracing::warn!(%err, "couldn't queue a budget notification");
            }
        }
    }

    /// The usage figures (`GET /usage`): totals, the last `days` days (1–90)
    /// day by day, and this month by kind, model, conversation and schedule.
    pub fn usage(&self, days: i64) -> Result<Value> {
        self.usage_at(days, Timestamp::now())
    }

    /// `usage`, as of `now`.
    fn usage_at(&self, days: i64, now: Timestamp) -> Result<Value> {
        let days = days.clamp(1, 90);
        let (timezone, tz) = zone(&self.store);
        let today = now.to_zoned(tz.clone()).date();
        let month_start = today.first_of_month();
        let last_month_start = month_start.checked_sub(1.month())?;
        let first_day = today.checked_sub((days - 1).days())?;
        let from = start_of(first_day.min(last_month_start), &tz)?;
        let rows = self.store.usage_since(&iso(from))?;

        let mut daily: BTreeMap<Date, (f64, i64)> = BTreeMap::new();
        let mut day = first_day;
        while day <= today {
            daily.insert(day, (0.0, 0));
            day = day.tomorrow()?;
        }
        let (mut today_cost, mut this_month, mut last_month) = (0.0, 0.0, 0.0);
        let mut by_kind: BTreeMap<&str, f64> = [("chat", 0.0), ("scheduled", 0.0), ("tidy_up", 0.0)].into();
        // model → (cost, runs, input, output, cached)
        let mut by_model: HashMap<String, (f64, i64, i64, i64, i64)> = HashMap::new();
        let mut by_session: HashMap<String, (f64, i64)> = HashMap::new();
        let mut by_schedule: HashMap<i64, (f64, i64)> = HashMap::new();
        for row in &rows {
            let date = Timestamp::from_second(row.started)
                .map_err(|err| anyhow!("{err}"))?
                .to_zoned(tz.clone())
                .date();
            let counted = i64::from(row.counted);
            if let Some((cost, runs)) = daily.get_mut(&date) {
                *cost += row.cost;
                *runs += counted;
            }
            if date == today {
                today_cost += row.cost;
            }
            if date < month_start {
                if date >= last_month_start {
                    last_month += row.cost;
                }
                continue;
            }
            this_month += row.cost;
            if let Some(kind) = by_kind.get_mut(row.kind.as_str()) {
                *kind += row.cost;
            }
            // A tidy-up that needed no model is "none": not a model.
            if row.model != "none" {
                let model = by_model.entry(row.model.clone()).or_default();
                model.0 += row.cost;
                model.1 += counted;
                model.2 += row.input;
                model.3 += row.output;
                model.4 += row.cached;
            }
            if let Some(session) = &row.session {
                let totals = by_session.entry(session.clone()).or_default();
                totals.0 += row.cost;
                totals.1 += counted;
            }
            if let Some(schedule) = row.schedule {
                let totals = by_schedule.entry(schedule).or_default();
                totals.0 += row.cost;
                totals.1 += counted;
            }
        }
        // Most expensive first.
        fn ranked<K>(totals: HashMap<K, (f64, i64)>) -> Vec<(K, (f64, i64))> {
            let mut list: Vec<_> = totals
                .into_iter()
                .filter(|(_, (cost, runs))| *cost > 0.0 || *runs > 0)
                .collect();
            list.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then(b.1.1.cmp(&a.1.1)));
            list
        }
        let titles: HashMap<String, String> = self.store.session_titles()?.into_iter().collect();
        let names: HashMap<i64, String> = self
            .store
            .schedules()?
            .into_iter()
            .map(|s| (s.id, s.name))
            .collect();
        let mut models: Vec<_> = by_model.into_iter().collect();
        models.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then(b.1.1.cmp(&a.1.1)));
        let budget = self.budget_at(now);
        Ok(json!({
            "currency": "USD",
            "estimated": true,
            "timezone": timezone,
            "month": today.strftime("%Y-%m").to_string(),
            "today": round(today_cost),
            "this_month": round(this_month),
            "last_month": round(last_month),
            "all_time": round(self.store.cost_all_time()?),
            "daily": daily.iter().map(|(date, (cost, runs))| json!({
                "date": date.to_string(), "cost": round(*cost), "runs": runs,
            })).collect::<Vec<_>>(),
            "by_kind": by_kind.iter().map(|(kind, cost)| (kind.to_string(), json!(round(*cost)))).collect::<serde_json::Map<_, _>>(),
            "by_model": models.iter().map(|(model, (cost, runs, input, output, cached))| json!({
                "model": model, "cost": round(*cost), "runs": runs,
                "input_tokens": input, "output_tokens": output, "cached_tokens": cached,
            })).collect::<Vec<_>>(),
            "by_session": ranked(by_session).into_iter().take(TOP).map(|(id, (cost, runs))| json!({
                "title": titles.get(&id).cloned().unwrap_or_default(), "id": id,
                "cost": round(cost), "runs": runs,
            })).collect::<Vec<_>>(),
            "by_schedule": ranked(by_schedule).into_iter().take(TOP).map(|(id, (cost, runs))| json!({
                "id": id, "name": names.get(&id), "cost": round(cost), "runs": runs,
            })).collect::<Vec<_>>(),
            "budget": {
                "monthly_usd": budget.monthly_usd, "spent": round(budget.spent),
                "fraction": budget.fraction().map(|f| (f * 1e4).round() / 1e4), "paused": budget.paused(),
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run as the usage figures see it: started at `at` (UTC).
    #[allow(clippy::too_many_arguments)]
    fn run(
        agent: &Agent,
        id: &str,
        at: &str,
        cost: Option<f64>,
        kind: &str,
        schedule: Option<i64>,
        session: Option<&str>,
        status: &str,
    ) {
        agent
            .store
            .transact(|tx| {
                tx.execute(
                    "INSERT INTO runs (id, kind, status, request, model, started, cost, steps, schedule, session,
                       input_tokens, output_tokens, cached_tokens)
                     VALUES (?1, ?2, ?3, 'x', 'zai-org/GLM-5.3', ?4, ?5, 1, ?6, ?7, 1000, 100, 10)",
                    rusqlite::params![id, kind, status, at, cost, schedule, session],
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn usage_is_counted_by_the_users_days_and_months() {
        let (agent, dir) = crate::agent::test_agent();
        agent
            .store
            .set_setting("user_timezone", "America/Toronto")
            .unwrap();
        let c1 = agent.create_session(Some("Lisbon")).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let schedule = agent
            .add_user_schedule("Morning inbox", "Summarize it", "0 8 * * *", None, None, true)
            .unwrap()["id"]
            .as_i64()
            .unwrap();
        // 23:30 on Monday November 23 in Toronto (EST, UTC-5).
        let now: Timestamp = "2026-11-24T04:30:00Z".parse().unwrap();
        // Toronto's November 23, though it's the 24th in UTC.
        run(
            &agent,
            "r1",
            "2026-11-24T01:00:00.000Z",
            Some(1.0),
            "task",
            None,
            Some("main"),
            "done",
        );
        run(
            &agent,
            "r2",
            "2026-11-23T03:00:00.000Z",
            Some(0.5),
            "task",
            Some(schedule),
            Some("main"),
            "done",
        );
        // October 31 in Toronto: last month, though November in UTC.
        run(
            &agent,
            "r3",
            "2026-11-01T03:00:00.000Z",
            Some(2.0),
            "task",
            None,
            Some(&c1),
            "done",
        );
        run(
            &agent,
            "r4",
            "2026-11-01T06:00:00.000Z",
            Some(0.25),
            "consolidation",
            None,
            None,
            "done",
        );
        run(
            &agent,
            "r5",
            "2026-10-15T12:00:00.000Z",
            Some(3.0),
            "task",
            None,
            Some("main"),
            "done",
        );
        run(
            &agent,
            "r6",
            "2026-09-10T12:00:00.000Z",
            Some(5.0),
            "task",
            None,
            Some("main"),
            "done",
        );
        // Queued and never run: no cost, not a run.
        run(
            &agent,
            "r7",
            "2026-11-23T05:00:00.000Z",
            None,
            "task",
            Some(schedule),
            Some("main"),
            "queued",
        );
        let usage = agent.usage_at(3, now).unwrap();
        assert_eq!(usage["timezone"], "America/Toronto");
        assert_eq!(usage["month"], "2026-11");
        assert_eq!(
            (
                &usage["today"],
                &usage["this_month"],
                &usage["last_month"],
                &usage["all_time"]
            ),
            (&json!(1.0), &json!(1.75), &json!(5.0), &json!(11.75))
        );
        assert_eq!(
            usage["daily"],
            json!([
                {"date": "2026-11-21", "cost": 0.0, "runs": 0},
                {"date": "2026-11-22", "cost": 0.5, "runs": 1},
                {"date": "2026-11-23", "cost": 1.0, "runs": 1},
            ])
        );
        assert_eq!(
            usage["by_kind"],
            json!({"chat": 1.0, "scheduled": 0.5, "tidy_up": 0.25})
        );
        assert_eq!(
            usage["by_model"],
            json!([{"model": "zai-org/GLM-5.3", "cost": 1.75, "runs": 3,
                    "input_tokens": 4000, "output_tokens": 400, "cached_tokens": 40}])
        );
        // This month's only: c1's run was last month.
        assert_eq!(
            usage["by_session"],
            json!([{"id": "main", "title": "First conversation", "cost": 1.5, "runs": 2}])
        );
        assert_eq!(
            usage["by_schedule"],
            json!([{"id": schedule, "name": "Morning inbox", "cost": 0.5, "runs": 1}])
        );
        // A deleted schedule keeps its line, without a name.
        agent.store.delete_schedule(schedule).unwrap();
        assert_eq!(
            agent.usage_at(3, now).unwrap()["by_schedule"][0]["name"],
            json!(null)
        );
        // The budget's month is the user's too.
        agent.store.set_setting("monthly_budget_usd", "2").unwrap();
        let budget = agent.budget_at(now);
        assert_eq!((budget.spent, budget.month.as_str()), (1.75, "2026-11"));
        assert!(!budget.paused());
        assert_eq!(budget.fraction(), Some(0.875));
        // Days default to 30 and stay within 1–90.
        assert_eq!(
            agent.usage_at(500, now).unwrap()["daily"]
                .as_array()
                .unwrap()
                .len(),
            90
        );
        assert_eq!(
            agent.usage_at(0, now).unwrap()["daily"].as_array().unwrap().len(),
            1
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_running_task_counts_as_it_goes() {
        let (agent, dir) = crate::agent::test_agent();
        agent
            .store
            .start_run("t1", "x", None, "zai-org/GLM-5.3", "v", None)
            .unwrap();
        let usage = crate::llm::Usage {
            input: 1_000_000,
            cached: 0,
            output: 0,
        };
        agent
            .store
            .record_usage("t1", usage.cost("zai-org/GLM-5.3"), &usage, 3)
            .unwrap();
        assert_eq!(agent.budget().spent, 1.4);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_budget_alerts_once_a_month_at_80_and_100_percent() {
        let (agent, dir) = crate::agent::test_agent();
        let alerts = |agent: &Agent| -> Vec<(i64, String)> {
            agent
                .store
                .events_after(0, 1000)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == "budget.alert")
                .map(|e| {
                    (
                        e.data["level"].as_i64().unwrap(),
                        e.data["month"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        };
        let now = Timestamp::now();
        let month = now.to_zoned(TimeZone::UTC).strftime("%Y-%m").to_string();
        // No budget: nothing to say.
        run(
            &agent,
            "a",
            &iso(now),
            Some(9.0),
            "task",
            None,
            Some("main"),
            "done",
        );
        agent.check_budget_at(now);
        assert!(alerts(&agent).is_empty());
        agent.store.set_setting("monthly_budget_usd", "10").unwrap();
        agent.check_budget_at(now);
        agent.check_budget_at(now);
        assert_eq!(alerts(&agent), [(80, month.clone())]);
        assert!(!agent.budget_paused());
        // Past 100%: that one once too, and new tasks wait.
        run(
            &agent,
            "b",
            &iso(now),
            Some(1.5),
            "task",
            None,
            Some("main"),
            "done",
        );
        agent.check_budget_at(now);
        agent.check_budget_at(now);
        assert_eq!(alerts(&agent), [(80, month.clone()), (100, month.clone())]);
        assert!(agent.budget_paused());
        let event = agent
            .store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "budget.alert" && e.data["level"] == 100)
            .unwrap();
        assert_eq!(
            (event.data["spent"].as_f64(), event.data["budget"].as_f64()),
            (Some(10.5), Some(10.0))
        );
        assert!(event.data.get("session").is_none(), "a global event");
        // A new month starts over: what was alerted last month doesn't count.
        agent
            .store
            .set_setting("budget_alerted", "2020-01:80,2020-01:100")
            .unwrap();
        agent.check_budget_at(now);
        assert_eq!(alerts(&agent).len(), 4);
        assert_eq!(
            agent.store.setting("budget_alerted").unwrap().as_deref(),
            Some(format!("{month}:80,{month}:100").as_str())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_budget_alert_notifies_the_apps() {
        let (agent, dir) = crate::agent::test_agent();
        agent
            .subscribe_push(
                "http://127.0.0.1:9/push/mac",
                "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
                "BTBZMqHH6r4Tts7J_aSIgg",
                "Mac",
            )
            .ok()
            .unwrap();
        agent.store.set_setting("monthly_budget_usd", "50").unwrap();
        run(
            &agent,
            "a",
            &iso(Timestamp::now()),
            Some(41.0),
            "task",
            None,
            Some("main"),
            "done",
        );
        agent.check_budget();
        let pushes: Vec<Value> = agent
            .store
            .due_effects(i64::MAX, 50)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "web_push")
            .map(|e| e.payload)
            .collect();
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0]["body"], "You've used 80% of this month's $50 budget");
        assert_eq!(pushes[0]["url"], "/#/chat");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
