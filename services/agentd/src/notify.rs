//! Notifications to the browsers where you turned them on in the web app
//! (Settings → Notifications), by Web Push (webpush.rs). What notifies you:
//! a card that needs you (an approval, a key, a site, a stuck task), Action
//! needed, a chat task that worked over a minute finishing (or failing), and a
//! scheduled run with something to report. Each is a `web_push` effect per
//! browser (effects.rs): stored first, sent with retries, and a newer one for
//! the same task replaces one not sent yet. Text goes out with saved keys cut
//! out, and is encrypted to the browser.

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::agent::Agent;
use crate::effects::Outcome;
use crate::store::{Effect, MAIN};
use crate::webpush::{self, Delivery, Vapid};

/// Browsers that can have notifications on at once.
const MAX_SUBSCRIPTIONS: usize = 20;
/// A notification not delivered within an hour isn't worth sending.
const STALE_SECS: i64 = 3600;
/// Failed tries before a notification is dropped (rather than left to show
/// as a problem on the desktop's status).
const MAX_TRIES: i64 = 5;
const TITLE_CHARS: usize = 80;
const BODY_CHARS: usize = 180;
/// Chat tasks shorter than this don't notify when they finish: you were likely
/// still looking.
const LONG_TASK_SECS: f64 = 60.0;

pub enum PushError {
    /// Bad endpoint or keys, too many browsers: tell the user.
    Invalid(String),
    Failed(anyhow::Error),
}

/// What a notification says, and how much it matters.
pub struct Notice {
    pub title: String,
    pub body: String,
    /// The task, or "test": a newer one with the same tag replaces it.
    pub tag: String,
    /// "high" for what waits on you, "normal" otherwise (RFC 8030).
    pub urgency: &'static str,
    /// How long the push service keeps it for an offline browser.
    pub ttl: u32,
    /// The app can open the corresponding task or card: `{task, approval?, action?}`.
    pub data: Value,
    /// The conversation it opens (none for a test or a budget alert).
    pub session: Option<String>,
}

impl Notice {
    /// A notification with only its task in `data` (a finished task or a test).
    pub fn plain(title: String, body: String, tag: String, urgency: &'static str, ttl: u32) -> Self {
        let data = json!({ "task": tag });
        Self {
            title,
            body,
            tag,
            urgency,
            ttl,
            data,
            session: None,
        }
    }
}

/// Where a Web Push notification opens the web app: its conversation, or
/// the chat (`main`) for one about none.
fn link(session: Option<&str>) -> String {
    match session {
        Some(session) => format!("/#/chat/{session}"),
        None => "/#/chat".into(),
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn clip(text: &str, chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= chars {
        flat
    } else {
        format!("{}…", flat.chars().take(chars - 1).collect::<String>().trim_end())
    }
}

/// An answer's first line of prose, without Markdown's marks (code blocks
/// and tables skipped).
pub fn first_line(text: &str) -> String {
    let mut in_code = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code || line.starts_with('|') {
            continue;
        }
        let line = line.trim_start_matches(['#', '>', '-', '*', ' ']).trim();
        if !line.is_empty() {
            return clip(&line.replace("**", "").replace('`', ""), BODY_CHARS);
        }
    }
    String::new()
}

/// What a card asks, as a notification: what waits on you.
pub fn card_notice(card: &Value) -> Option<(String, String)> {
    let text = |key: &str| card[key].as_str().map(str::trim).filter(|t| !t.is_empty());
    let (title, body) = match card["kind"].as_str()? {
        "shell" => (
            "Needs your OK".to_string(),
            text("summary").or(text("command"))?.to_string(),
        ),
        "browser" => match text("action")? {
            "type" => ("Write this?".into(), text("text")?.to_string()),
            "key" => (
                format!("Press {}?", text("key").unwrap_or("a key")),
                text("summary").or(text("target")).unwrap_or_default().to_string(),
            ),
            _ => (
                "Click this?".into(),
                text("summary").or(text("target"))?.to_string(),
            ),
        },
        "schedule" => (
            "Schedule this?".into(),
            text("name").or(text("text"))?.to_string(),
        ),
        "secret" => ("Needs a key".into(), text("label").or(text("name"))?.to_string()),
        "access" => {
            let hosts: Vec<&str> = card["hosts"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            ("Let it reach a site?".into(), hosts.join(", "))
        }
        "stuck" => ("Looks stuck".into(), text("reason")?.to_string()),
        _ => return None,
    };
    Some((clip(&title, TITLE_CHARS), clip(&body, BODY_CHARS)))
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| webpush::client().unwrap_or_default())
}

/// An HTTP/2 client for APNs (Apple requires HTTP/2; the Web Push one is HTTP/1).
fn apns_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default()
    })
}

/// A bundle id's bytes (letters, digits, dots and hyphens).
fn valid_topic_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'-'
}

impl Agent {
    /// This desktop's VAPID key, made the first time it's needed and kept in
    /// agentd's settings (a reset makes a new one, with the subscriptions gone).
    pub fn vapid(&self) -> Result<Vapid> {
        static MAKING: Mutex<()> = Mutex::new(());
        let _one_at_a_time = MAKING.lock().unwrap();
        if let Some(saved) = self.store.setting("vapid_pkcs8")? {
            return Vapid::from_pkcs8(&STANDARD.decode(saved).context("the saved VAPID key")?);
        }
        let (vapid, pkcs8) = Vapid::generate()?;
        self.store.set_setting("vapid_pkcs8", &STANDARD.encode(pkcs8))?;
        tracing::info!("made this desktop's VAPID key for notifications");
        Ok(vapid)
    }

    /// For the web app: the key browsers subscribe with, the browsers on, and
    /// the apps registered for notifications (apns.rs). `apns` says whether an
    /// APNs key is configured, so the apps can tell "off" from "no device yet".
    pub fn push_info(&self) -> Result<Value> {
        Ok(json!({
            "public_key": self.vapid()?.public_key(),
            "subscriptions": self.store.push_subscriptions()?,
            "apns": self.apns.is_some(),
            "devices": self.store.device_tokens()?,
        }))
    }

    /// An app registers its device token for APNs notifications (its own bundle
    /// id is the topic). Replaces the entry with the same token.
    pub fn register_device(
        &self,
        token: &str,
        topic: &str,
        environment: &str,
        label: &str,
    ) -> Result<String, PushError> {
        if self.apns.is_none() {
            return Err(PushError::Invalid(
                "this desktop has no APNs key: notifications to the apps are off".into(),
            ));
        }
        let token = token.trim();
        if token.is_empty() || token.len() > 200 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(PushError::Invalid("the device token must be hex".into()));
        }
        let topic = topic.trim();
        if topic.is_empty() || topic.len() > 200 || !topic.bytes().all(valid_topic_byte) {
            return Err(PushError::Invalid(
                "the app's bundle id (topic) is missing or invalid".into(),
            ));
        }
        let environment = match environment {
            "sandbox" | "production" => environment,
            _ => "production",
        };
        let id = self
            .store
            .save_device_token(token, topic, environment, &clip(label, 60), unix_now())
            .map_err(PushError::Failed)?;
        tracing::info!(device = %id, environment, "notifications on in an app");
        Ok(id)
    }

    pub fn unregister_device(&self, id: &str) -> Result<bool> {
        let gone = self.store.delete_device_token(id)?;
        if gone {
            tracing::info!(device = %id, "notifications off in an app");
        }
        Ok(gone)
    }

    pub fn subscribe_push(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        label: &str,
    ) -> Result<String, PushError> {
        let url =
            webpush::check_endpoint(endpoint, self.cfg.push_dev_endpoints).map_err(PushError::Invalid)?;
        webpush::parse_keys(p256dh, auth).map_err(PushError::Invalid)?;
        let existing = self.store.push_subscriptions().map_err(PushError::Failed)?;
        if existing.len() >= MAX_SUBSCRIPTIONS && !existing.iter().any(|s| s.endpoint == url.as_str()) {
            return Err(PushError::Invalid(format!(
                "notifications are on in {MAX_SUBSCRIPTIONS} browsers already: remove one first"
            )));
        }
        let label = clip(label, 60);
        let id = self
            .store
            .save_push_subscription(url.as_str(), p256dh.trim(), auth.trim(), &label, unix_now())
            .map_err(PushError::Failed)?;
        tracing::info!(subscription = %id, label = %label, "notifications on in a browser");
        Ok(id)
    }

    pub fn unsubscribe_push(&self, id: &str) -> Result<bool> {
        let gone = self.store.delete_push_subscription(id)?;
        if gone {
            tracing::info!(subscription = %id, "notifications off in a browser");
        }
        Ok(gone)
    }

    /// Queues a notification for every browser and app with notifications on
    /// (or only the one whose id is `only`). How many it went to. Each is a
    /// `web_push` or `apns` effect (effects.rs): stored first, sent with
    /// retries, and a newer one for the same task tag replaces one not sent yet.
    pub fn notify(&self, notice: &Notice, only: Option<&str>) -> Result<usize> {
        let now = unix_now();
        let title = self.secrets.redact(&notice.title);
        let body = self.secrets.redact(&notice.body);
        let mut queued = 0;
        for sub in self.store.push_subscriptions()? {
            if only.is_some_and(|id| id != sub.id) {
                continue;
            }
            let payload = json!({
                "sub": sub.id, "title": title, "body": body,
                "tag": notice.tag, "url": link(notice.session.as_deref()), "urgency": notice.urgency,
                "ttl": notice.ttl, "created": now,
            });
            self.enqueue("web_push", &format!("{}:{}", sub.id, notice.tag), &payload, now)?;
            queued += 1;
        }
        if self.apns.is_some() {
            // The app opens the notice's conversation.
            let mut data = notice.data.clone();
            if let (Some(session), Some(fields)) = (&notice.session, data.as_object_mut()) {
                fields.insert("session".into(), json!(session));
            }
            for device in self.store.device_tokens()? {
                if only.is_some_and(|id| id != device.id) {
                    continue;
                }
                let payload = json!({
                    "device": device.id, "title": title, "body": body, "tag": notice.tag,
                    "urgency": notice.urgency, "ttl": notice.ttl, "created": now,
                    "data": data,
                });
                self.enqueue("apns", &format!("{}:{}", device.id, notice.tag), &payload, now)?;
                queued += 1;
            }
        }
        Ok(queued)
    }

    /// Sends a task's notice: about its conversation, titled with the
    /// conversation's title first when it isn't `main`.
    fn notify_quietly(&self, mut notice: Notice) {
        let task = notice.data["task"].as_str().unwrap_or(&notice.tag).to_string();
        notice.session = self.session_of(&task);
        if let Some(session) = notice.session.as_deref().filter(|s| *s != MAIN)
            && let Some(row) = self.store.session(session).ok().flatten()
            && !row.title.is_empty()
        {
            notice.title = clip(&format!("{}: {}", row.title, notice.title), TITLE_CHARS);
        }
        if let Err(err) = self.notify(&notice, None) {
            tracing::warn!(%err, "couldn't queue a notification");
        }
    }

    /// A parked task's card waits for review in the app.
    pub fn notify_card(&self, task: &str, card: &Value) {
        if let Some((title, body)) = card_notice(card) {
            let id = card["id"].as_str().unwrap_or_default();
            self.notify_quietly(Notice {
                title,
                body,
                tag: task.to_string(),
                urgency: "high",
                ttl: 86_400,
                data: json!({"task": task, "approval": id}),
                session: None,
            });
        }
    }

    pub fn notify_action_needed(&self, task: &str, id: &str, reason: &str) {
        self.notify_quietly(Notice {
            title: "Needs you".into(),
            body: clip(reason, BODY_CHARS),
            tag: task.to_string(),
            urgency: "high",
            ttl: 86_400,
            data: json!({"task": task, "action": id}),
            session: None,
        });
    }

    /// A task ended. A chat task notifies only after working over a minute
    /// (done or failed); a scheduled run when it has something to report.
    #[allow(clippy::too_many_arguments)]
    pub fn notify_finished(
        &self,
        task: &str,
        status: &str,
        seconds: f64,
        answer: &str,
        schedule: Option<&str>,
        summary: Option<&str>,
        quiet: bool,
    ) {
        let body = summary
            .map(|s| clip(s, BODY_CHARS))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| first_line(answer));
        let title = match (schedule, status) {
            (Some(_), _) if quiet || status == "cancelled" => return,
            (Some(name), _) => clip(name, TITLE_CHARS),
            (None, "done") if seconds >= LONG_TASK_SECS => "Done".to_string(),
            (None, "error") => "Hit a problem".to_string(),
            _ => return,
        };
        self.notify_quietly(Notice::plain(title, body, task.to_string(), "normal", 4 * 3600));
    }
}

/// Sends one `web_push` effect: encrypted to its browser, signed with this
/// desktop's key.
pub async fn deliver(agent: &Agent, effect: &Effect) -> Outcome {
    let p = &effect.payload;
    let now = unix_now();
    let Some(sub) = p["sub"]
        .as_str()
        .and_then(|id| agent.store.push_subscription(id).ok().flatten())
    else {
        return Outcome::Done; // notifications were turned off there
    };
    if now - p["created"].as_i64().unwrap_or(0) > STALE_SECS || effect.attempts >= MAX_TRIES {
        tracing::warn!(subscription = %sub.id, attempts = effect.attempts, "notification dropped: not delivered in time");
        return Outcome::Done;
    }
    match send(agent, &sub, p, now).await {
        Ok(Delivery::Sent) => {
            let _ = agent.store.push_sent(&sub.id, now);
            Outcome::Done
        }
        Ok(Delivery::Gone) => {
            tracing::info!(subscription = %sub.id, "the browser's subscription is gone: removed");
            let _ = agent.store.delete_push_subscription(&sub.id);
            Outcome::Done
        }
        Ok(Delivery::Refused(why)) => {
            tracing::warn!(subscription = %sub.id, %why, "notification refused");
            Outcome::Done
        }
        Ok(Delivery::Retry(Some(after), _)) => {
            Outcome::Later(after.clamp(Duration::from_secs(2), Duration::from_secs(600)))
        }
        Ok(Delivery::Retry(None, why)) => Outcome::Failed(why),
        Err(err) => {
            tracing::warn!(subscription = %sub.id, err = %format!("{err:#}"), "couldn't make the notification");
            Outcome::Done
        }
    }
}

async fn send(agent: &Agent, sub: &crate::store::PushSubscription, p: &Value, now: i64) -> Result<Delivery> {
    let endpoint =
        webpush::check_endpoint(&sub.endpoint, agent.cfg.push_dev_endpoints).map_err(|e| anyhow!(e))?;
    let keys = webpush::parse_keys(&sub.p256dh, &sub.auth).map_err(|e| anyhow!(e))?;
    let mut message = json!({"title": p["title"], "body": p["body"], "tag": p["tag"], "url": p["url"]});
    if message.to_string().len() > webpush::MAX_PAYLOAD {
        message["body"] = json!(clip(p["body"].as_str().unwrap_or_default(), 120));
    }
    let body = webpush::encrypt(message.to_string().as_bytes(), &keys)?;
    let authorization = agent
        .vapid()?
        .authorization(&endpoint, &agent.cfg.vapid_subject, now)?;
    let urgency = if p["urgency"] == "high" { "high" } else { "normal" };
    let ttl = p["ttl"].as_u64().unwrap_or(86_400) as u32;
    Ok(webpush::send(client(), &endpoint, body, &authorization, urgency, ttl).await)
}

/// Sends one `apns` effect to its iPhone or Mac app, through Apple (apns.rs).
pub async fn deliver_apns(agent: &Agent, effect: &Effect) -> Outcome {
    let p = &effect.payload;
    let now = unix_now();
    let Some(apns) = agent.apns.as_ref() else {
        return Outcome::Done; // no APNs key: notifications to the apps are off
    };
    let Some(device) = p["device"]
        .as_str()
        .and_then(|id| agent.store.device_token(id).ok().flatten())
    else {
        return Outcome::Done; // the app turned notifications off
    };
    if now - p["created"].as_i64().unwrap_or(0) > STALE_SECS || effect.attempts >= MAX_TRIES {
        tracing::warn!(device = %device.id, attempts = effect.attempts, "notification dropped: not delivered in time");
        return Outcome::Done;
    }
    let urgency = if p["urgency"] == "high" { "high" } else { "normal" };
    let priority = if urgency == "high" { "10" } else { "5" };
    let ttl = p["ttl"].as_i64().unwrap_or(86_400);
    let message = apns_message(p);
    let production = device.environment != "sandbox";
    let delivery = apns
        .send(
            apns_client(),
            &device.token,
            &device.topic,
            production,
            &message,
            p["tag"].as_str().unwrap_or_default(),
            priority,
            now + ttl,
            now,
        )
        .await;
    match delivery {
        crate::apns::Delivery::Sent => {
            let _ = agent.store.device_token_sent(&device.id, now);
            Outcome::Done
        }
        crate::apns::Delivery::Gone => {
            tracing::info!(device = %device.id, "the app's token is gone: removed");
            let _ = agent.store.delete_device_token(&device.id);
            Outcome::Done
        }
        crate::apns::Delivery::Refused(why) => {
            tracing::warn!(device = %device.id, %why, "notification refused by Apple");
            Outcome::Done
        }
        crate::apns::Delivery::Retry(Some(after), _) => {
            Outcome::Later(after.clamp(Duration::from_secs(2), Duration::from_secs(600)))
        }
        crate::apns::Delivery::Retry(None, why) => Outcome::Failed(why),
    }
}

/// The APNs payload: the `aps` dictionary (alert, sound, thread) and
/// the app's own keys (task, approval or action) at the top level.
fn apns_message(p: &Value) -> Value {
    let aps = json!({
        "alert": {"title": p["title"], "body": p["body"]},
        "sound": "default",
        "thread-id": p["tag"],
    });
    let mut message = json!({ "aps": aps });
    if let Some(data) = p["data"].as_object() {
        for (key, value) in data {
            message[key] = value.clone();
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::webpush::tests::{d, decrypt};
    use axum::Router;
    use axum::routing::post;
    use std::sync::Arc;

    /// RFC 8291's browser keys: what a test browser subscribes with.
    const P256DH: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const UA_PRIVATE: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";

    /// A stand-in push service: keeps each push; `/push/gone` answers 410.
    async fn push_service() -> (
        String,
        Arc<std::sync::Mutex<Vec<(String, axum::http::HeaderMap, Vec<u8>)>>>,
    ) {
        let got = Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = got.clone();
        let app = Router::new().route(
            "/push/{name}",
            post(
                move |axum::extract::Path(name): axum::extract::Path<String>,
                      headers: axum::http::HeaderMap,
                      body: axum::body::Bytes| {
                    let kept = kept.clone();
                    async move {
                        let status = if name == "gone" { 410 } else { 201 };
                        kept.lock().unwrap().push((name, headers, body.to_vec()));
                        axum::http::StatusCode::from_u16(status).unwrap()
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, got)
    }

    /// Delivers what's queued, as the effects runner would.
    async fn deliver_all(agent: &Agent) {
        for effect in agent.store.due_effects(i64::MAX, 50).unwrap() {
            if effect.kind == "web_push" {
                assert_eq!(deliver(agent, &effect).await, Outcome::Done);
                agent.store.effect_done(&effect).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn a_notification_reaches_the_browser_encrypted_and_signed() {
        let (base, got) = push_service().await;
        let (agent, dir) = crate::agent::test_agent();
        let id = agent
            .subscribe_push(&format!("{base}/push/mac"), P256DH, AUTH, "Mac · Chrome")
            .ok()
            .unwrap();
        let info = agent.push_info().unwrap();
        assert_eq!(info["subscriptions"][0]["id"], id);
        assert_eq!(info["subscriptions"][0]["label"], "Mac · Chrome");
        assert!(
            info["subscriptions"][0].get("p256dh").is_none(),
            "keys stay on the desktop"
        );
        let public = info["public_key"].as_str().unwrap().to_string();

        // A card waits on you: the notification says what.
        agent.notify_card(
            "t1",
            &json!({"kind": "shell", "summary": "Clear the old build", "command": "rm -rf ./old"}),
        );
        deliver_all(&agent).await;
        let pushes = got.lock().unwrap();
        assert_eq!(pushes.len(), 1);
        let (name, headers, body) = &pushes[0];
        assert_eq!(name, "mac");
        assert_eq!(headers["content-encoding"], "aes128gcm");
        assert_eq!(headers["urgency"], "high");
        assert_eq!(headers["ttl"], "86400");
        let keys = webpush::parse_keys(P256DH, AUTH).unwrap();
        let message: Value = serde_json::from_slice(&decrypt(body, &d(UA_PRIVATE), &keys)).unwrap();
        assert_eq!(
            message,
            json!({"title": "Needs your OK", "body": "Clear the old build", "tag": "t1", "url": "/#/chat"})
        );
        // Signed with the key the browser subscribed with, for this origin.
        let authorization = headers["authorization"].to_str().unwrap();
        let (token, key) = authorization
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(key, public);
        let parts: Vec<&str> = token.split('.').collect();
        let claims: Value = serde_json::from_slice(&d(parts[1])).unwrap();
        assert_eq!(claims["aud"], base);
        assert_eq!(claims["sub"], "mailto:test@example.com");
        aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED, d(key))
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &d(parts[2]))
            .unwrap();
        drop(pushes);
        assert!(agent.push_info().unwrap()["subscriptions"][0]["last_sent"].is_i64());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_saved_keys_value_never_goes_out_and_a_gone_browser_is_forgotten() {
        let (base, got) = push_service().await;
        let (agent, dir) = crate::agent::test_agent();
        agent
            .secrets
            .put("GITHUB_TOKEN", "ghp_abcdefghijklmnopqrstuvwxyz0123456789")
            .unwrap();
        agent
            .subscribe_push(&format!("{base}/push/gone"), P256DH, AUTH, "Old phone")
            .ok()
            .unwrap();
        agent.notify_action_needed(
            "t2",
            "ac1",
            "Sign in with ghp_abcdefghijklmnopqrstuvwxyz0123456789 please",
        );
        deliver_all(&agent).await;
        let keys = webpush::parse_keys(P256DH, AUTH).unwrap();
        let message: Value =
            serde_json::from_slice(&decrypt(&got.lock().unwrap()[0].2, &d(UA_PRIVATE), &keys)).unwrap();
        assert_eq!(message["body"], "Sign in with [secret GITHUB_TOKEN] please");
        // The service said 410: that browser is gone.
        assert!(
            agent.push_info().unwrap()["subscriptions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn only_what_matters_notifies() {
        let (base, _) = push_service().await;
        let (agent, dir) = crate::agent::test_agent();
        agent
            .subscribe_push(&format!("{base}/push/mac"), P256DH, AUTH, "Mac")
            .ok()
            .unwrap();
        let queued = || {
            agent
                .store
                .due_effects(i64::MAX, 50)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "web_push")
                .count()
        };
        agent.notify_finished("quick", "done", 12.0, "Hi there.", None, None, false);
        agent.notify_finished("stopped", "cancelled", 300.0, "Stopped.", None, None, false);
        agent.notify_finished("quiet", "done", 300.0, "Nothing new.", Some("Inbox"), None, true);
        assert_eq!(queued(), 0, "a quick answer, a stop and a quiet run don't notify");
        agent.notify_finished("long", "done", 300.0, "## Done\nAll of it.", None, None, false);
        agent.notify_finished(
            "run",
            "done",
            30.0,
            "Long text",
            Some("Inbox"),
            Some("Two emails need replies"),
            false,
        );
        let payloads: Vec<Value> = agent
            .store
            .due_effects(i64::MAX, 50)
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .collect();
        assert_eq!(payloads.len(), 2);
        assert_eq!(
            (payloads[0]["title"].as_str(), payloads[0]["body"].as_str()),
            (Some("Done"), Some("Done"))
        );
        assert_eq!(
            (payloads[1]["title"].as_str(), payloads[1]["body"].as_str()),
            (Some("Inbox"), Some("Two emails need replies"))
        );
        // Bad subscriptions are refused.
        assert!(matches!(
            agent.subscribe_push("https://evil.example.com/x", P256DH, AUTH, "x"),
            Err(PushError::Invalid(_))
        ));
        assert!(matches!(
            agent.subscribe_push(&format!("{base}/push/b"), "AAAA", AUTH, "x"),
            Err(PushError::Invalid(_))
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cards_say_what_waits_on_you() {
        let n = |card: Value| card_notice(&card).unwrap();
        assert_eq!(
            n(json!({"kind": "shell", "summary": "Clear the old build", "command": "rm -rf ./old"})),
            ("Needs your OK".into(), "Clear the old build".into())
        );
        assert_eq!(
            n(json!({"kind": "shell", "command": "sudo apt install jq"})).1,
            "sudo apt install jq"
        );
        assert_eq!(
            n(json!({"kind": "browser", "action": "type", "text": "Hi Lawrence", "sends": true})).0,
            "Write this?"
        );
        assert_eq!(
            n(
                json!({"kind": "browser", "action": "click", "summary": "Pay the March invoice", "target": "Confirm"})
            ),
            ("Click this?".into(), "Pay the March invoice".into())
        );
        assert_eq!(
            n(json!({"kind": "secret", "label": "GitHub token", "name": "GITHUB_TOKEN"})).1,
            "GitHub token"
        );
        assert_eq!(
            n(json!({"kind": "access", "hosts": ["api.example.com", "cdn.example.com"]})).1,
            "api.example.com, cdn.example.com"
        );
        assert_eq!(
            n(json!({"kind": "stuck", "reason": "The build fails the same way each time."})),
            (
                "Looks stuck".into(),
                "The build fails the same way each time.".into()
            )
        );
        assert!(card_notice(&json!({"kind": "mystery"})).is_none());
        assert_eq!(
            n(json!({"kind": "shell", "summary": "word ".repeat(80)}))
                .1
                .chars()
                .count(),
            BODY_CHARS
        );
    }

    #[test]
    fn an_answer_notifies_with_its_first_line() {
        assert_eq!(first_line("## Here's the report\n\n- more"), "Here's the report");
        assert_eq!(
            first_line("\n\n**Three things** need `you`.\nRest."),
            "Three things need you."
        );
        assert_eq!(first_line("```bash\nls\n```\n| a | b |\nDone."), "Done.");
    }

    #[test]
    fn the_apns_payload_opens_the_card_and_discards_legacy_approval_actions() {
        // A queued notification from an earlier version may still carry the category.
        let effect = json!({
            "title": "Needs your OK", "body": "rm -rf build", "tag": "t1",
            "category": "APPROVAL", "data": {"task": "t1", "approval": "a9"},
        });
        let message = apns_message(&effect);
        assert_eq!(message["aps"]["alert"]["title"], "Needs your OK");
        assert_eq!(message["aps"]["alert"]["body"], "rm -rf build");
        assert_eq!(message["aps"]["thread-id"], "t1");
        assert!(message["aps"].get("category").is_none());
        assert_eq!(message["aps"]["sound"], "default");
        assert_eq!(message["approval"], "a9");
        assert_eq!(message["task"], "t1");

        // A finished task: no category, no buttons, just the task.
        let plain = apns_message(&json!({
            "title": "Done", "body": "All set.", "tag": "t2", "category": null, "data": {"task": "t2"},
        }));
        assert!(plain["aps"].get("category").is_none());
        assert_eq!(plain["task"], "t2");
    }

    #[test]
    fn a_registered_app_gets_an_apns_effect_per_notification() {
        let (agent, dir) = crate::agent::test_agent_with_apns();
        assert!(agent.apns.is_some());
        // A bad token is refused; a good one registers with its bundle id.
        assert!(matches!(
            agent.register_device("not-hex!", "dev.neoagent.app.x", "sandbox", "iPhone"),
            Err(PushError::Invalid(_))
        ));
        let id = agent
            .register_device("a1b2c3d4", "dev.neoagent.app.x", "sandbox", "iPhone 17")
            .ok()
            .unwrap();
        assert_eq!(agent.push_info().unwrap()["devices"][0]["id"], id);
        assert_eq!(agent.push_info().unwrap()["devices"][0]["label"], "iPhone 17");
        assert_eq!(agent.push_info().unwrap()["apns"], true);
        // Keys stay on the desktop: the token and topic aren't listed.
        assert!(agent.push_info().unwrap()["devices"][0].get("token").is_none());

        // A command card: one apns effect, with the id for the app to open.
        agent.notify_card(
            "t1",
            &json!({"kind": "shell", "id": "a9", "summary": "Clear the build", "command": "rm -rf build"}),
        );
        let effects: Vec<_> = agent
            .store
            .due_effects(i64::MAX, 50)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "apns")
            .collect();
        assert_eq!(effects.len(), 1);
        let p = &effects[0].payload;
        assert_eq!(p["device"], id);
        assert_eq!(p["title"], "Needs your OK");
        assert!(p.get("category").is_none());
        assert_eq!(p["data"]["approval"], "a9");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn with_no_apns_key_the_apps_cant_register_and_nothing_is_sent() {
        let (agent, dir) = crate::agent::test_agent();
        // The test agent has no APNs key (APNS_AUTH_KEY empty).
        assert!(agent.apns.is_none());
        assert!(matches!(
            agent.register_device("abc123", "dev.neoagent.app.x", "sandbox", "iPhone"),
            Err(PushError::Invalid(_))
        ));
        // notify() queues web_push only; no apns effects without a key.
        agent.notify_action_needed("t1", "a1", "Sign in");
        let apns = agent
            .store
            .due_effects(i64::MAX, 50)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "apns")
            .count();
        assert_eq!(apns, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
