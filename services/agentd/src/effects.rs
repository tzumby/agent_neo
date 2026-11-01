//! Side effects, stored first and done afterwards (docs/memory-v2.md §9).
//!
//! An effect is a row in the `effects` table, written in the same transaction
//! as the state change that calls for it, then done by this runner with
//! retries. Nothing is fire-and-forget: a row survives restarts and is simply
//! due again. One row per (kind, key): enqueuing a waiting key replaces its
//! payload and due time, so a burst of triggers does the work once.
//!
//! The runner wakes every 2 s, or at once when something is enqueued, and runs
//! due rows oldest first, at most one at a time per kind. A handler answers:
//! - `Done`: the row is removed;
//! - `Later(d)`: due again after `d`, not counted as a failure;
//! - `Failed(e)`: due again after a backoff that doubles per failure. After
//!   `EFFECT_MAX_ATTEMPTS` failures the row stays, given up, and `/status`
//!   reports it; enqueuing the same key again starts it over.
//!
//! Kinds: `purge` (daily), `consolidate` (consolidate.rs) and `push` (a
//! notification through the relay).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::agent::Agent;
use crate::store::{EFFECT_MAX_ATTEMPTS, Effect, Store};

/// How often the runner looks for due effects without being woken.
const TICK: Duration = Duration::from_secs(2);
/// Deleted memories are kept this long (restorable) before the purge.
const PURGE_AFTER_SECS: i64 = 30 * 86_400;
const DAY: Duration = Duration::from_secs(86_400);

/// What a handler did.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Done,
    Later(Duration),
    Failed(String),
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The wait before the next try after `attempts` failures: doubling from a
/// first wait to a cap, per kind.
pub fn backoff(kind: &str, attempts: i64) -> Duration {
    let (first, cap) = match kind {
        "push" => (5, 300),
        _ => (2, 60),
    };
    let doublings = attempts.saturating_sub(1).clamp(0, 16) as u32;
    Duration::from_secs((first * 2u64.pow(doublings)).min(cap))
}

/// Runs effects until agentd stops. Started once the socket is bound, next to
/// the scheduler, so two agentd processes never run the same effect.
pub async fn run(agent: Arc<Agent>) {
    // The daily purge is always there; the first runs a minute after startup.
    if let Err(err) = agent
        .store
        .ensure_effect("purge", "daily", &json!({}), unix_now() + 60)
    {
        tracing::error!(%err, "couldn't schedule the daily purge");
    }
    let running: Arc<Mutex<HashSet<String>>> = Arc::default();
    loop {
        let due = agent.store.due_effects(unix_now(), 50).unwrap_or_else(|err| {
            tracing::error!(%err, "couldn't read the effects queue");
            Vec::new()
        });
        for effect in due {
            // At most one at a time per kind.
            if !running.lock().unwrap().insert(effect.kind.clone()) {
                continue;
            }
            let agent = agent.clone();
            let running = running.clone();
            tokio::spawn(async move {
                let outcome = handle(&agent, &effect).await;
                settle(&agent.store, &effect, outcome);
                running.lock().unwrap().remove(&effect.kind);
                // Something of this kind may be waiting behind it.
                agent.effects_changed.notify_one();
            });
        }
        let next = agent.store.next_effect_due().ok().flatten();
        let wait = next
            .map(|due| Duration::from_secs((due - unix_now()).clamp(0, TICK.as_secs() as i64) as u64))
            .unwrap_or(TICK)
            .max(Duration::from_millis(200));
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = agent.effects_changed.notified() => {}
        }
    }
}

/// Records what a handler did.
fn settle(store: &Store, effect: &Effect, outcome: Outcome) {
    let result = match &outcome {
        Outcome::Done => store.effect_done(effect),
        Outcome::Later(after) => store.effect_again(effect, unix_now() + after.as_secs() as i64, None),
        Outcome::Failed(error) => {
            let attempts = effect.attempts + 1;
            if attempts >= EFFECT_MAX_ATTEMPTS {
                // Kind and key only: payloads may hold user content.
                tracing::error!(kind = %effect.kind, key = %effect.key, attempts, "effect gave up");
            } else {
                tracing::warn!(kind = %effect.kind, key = %effect.key, attempts, "effect failed; will retry");
            }
            let due = unix_now() + backoff(&effect.kind, attempts).as_secs() as i64;
            store.effect_again(effect, due, Some(error))
        }
    };
    if let Err(err) = result {
        tracing::error!(%err, kind = %effect.kind, key = %effect.key, "couldn't record an effect's outcome");
    }
}

async fn handle(agent: &Agent, effect: &Effect) -> Outcome {
    match effect.kind.as_str() {
        "purge" => purge(&agent.store),
        "consolidate" => crate::consolidate::run(agent).await,
        "push" => push(agent, effect).await,
        // From a newer agentd (after a rollback): keep it for that version.
        other => Outcome::Failed(format!("unknown effect kind {other}")),
    }
}

/// Sends a notification through the push relay. With no relay configured
/// (any more), there's nothing to do.
async fn push(agent: &Agent, effect: &Effect) -> Outcome {
    let Some(relay) = agent.push.clone() else {
        return Outcome::Done;
    };
    let Some(notification) = crate::push::Push::from_payload(&effect.payload) else {
        return Outcome::Done; // malformed: retrying wouldn't help
    };
    match relay.notify(&notification).await {
        Ok(()) => Outcome::Done,
        Err(err) => Outcome::Failed(format!("{err:#}")),
    }
}

/// Hard-deletes memories deleted more than 30 days ago, then runs again the
/// next day.
fn purge(store: &Store) -> Outcome {
    match store.purge_deleted_memories(unix_now() - PURGE_AFTER_SECS) {
        Ok(purged) => {
            if purged > 0 {
                tracing::info!(purged, "purged deleted memories");
            }
            Outcome::Later(DAY)
        }
        Err(err) => Outcome::Failed(format!("{err:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    fn only(store: &Store) -> Effect {
        let (_, failing) = store.effects_health().unwrap();
        let mut due = store.due_effects(i64::MAX, 10).unwrap();
        due.extend(failing);
        assert_eq!(due.len(), 1, "{due:?}");
        due.remove(0)
    }

    #[test]
    fn outcomes_remove_redo_or_count_against_an_effect() {
        let store = store();
        let now = unix_now();
        store
            .ensure_effect("push", "task:t1:none", &json!({"title": "Done"}), now)
            .unwrap();

        // Later: due again, not a failure.
        settle(&store, &only(&store), Outcome::Later(Duration::from_secs(60)));
        let later = only(&store);
        assert_eq!(later.attempts, 0);
        assert!(later.due >= now + 60 && store.due_effects(now, 10).unwrap().is_empty());

        // Failed: counts, backs off, keeps the error.
        settle(&store, &later, Outcome::Failed("relay answered 502".into()));
        let failed = only(&store);
        assert_eq!(
            (failed.attempts, failed.last_error.as_deref()),
            (1, Some("relay answered 502"))
        );
        assert!(failed.due >= now + 5);

        // Ten failures: given up, reported, no longer due.
        let mut effect = failed;
        for _ in 1..EFFECT_MAX_ATTEMPTS {
            settle(&store, &effect, Outcome::Failed("still down".into()));
            effect = only(&store);
        }
        let (waiting, failing) = store.effects_health().unwrap();
        assert_eq!(
            (waiting, failing.len(), failing[0].attempts),
            (0, 1, EFFECT_MAX_ATTEMPTS)
        );
        assert!(store.due_effects(i64::MAX, 10).unwrap().is_empty());

        // Enqueued again, it starts over; Done removes it.
        store
            .enqueue_effect("push", "task:t1:none", &json!({"title": "Again"}), now)
            .unwrap();
        let fresh = only(&store);
        assert_eq!((fresh.attempts, fresh.last_error.clone()), (0, None));
        settle(&store, &fresh, Outcome::Done);
        assert_eq!(store.effects_health().unwrap(), (0, vec![]));
    }

    #[test]
    fn the_purge_comes_back_daily() {
        let store = store();
        store
            .ensure_effect("purge", "daily", &json!({}), unix_now())
            .unwrap();
        let effect = only(&store);
        settle(&store, &effect, purge(&store));
        let next = only(&store);
        assert_eq!(next.attempts, 0);
        assert!(next.due >= unix_now() + DAY.as_secs() as i64 - 5);
    }

    #[test]
    fn backoff_doubles_to_a_cap_per_kind() {
        let waits = |kind| (1..=8).map(|n| backoff(kind, n).as_secs()).collect::<Vec<_>>();
        assert_eq!(waits("consolidate"), [2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(waits("purge"), [2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(waits("push"), [5, 10, 20, 40, 80, 160, 300, 300]);
        assert_eq!(backoff("push", 0).as_secs(), 5);
    }
}
