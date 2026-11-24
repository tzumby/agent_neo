//! Apple Push Notification service (APNs) from agentd itself, to the iPhone and
//! Mac apps (apps/apple): no push relay, no app-store server. agentd holds your
//! APNs auth key (a `.p8` from your Apple developer account), the same trust as
//! the model key, and signs each request with it (a JWT, ES256). The apps
//! register their device tokens through the API (notify.rs), and a notification
//! goes to Apple over HTTP/2, encrypted in transit by TLS.
//!
//! This is the apple half of the notification system; webpush.rs is the browser
//! half. notify.rs decides what to say and queues an effect per device.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::rand as lc_rand;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};

/// Apple's production and development push hosts. A token registered by an
/// Xcode build belongs to the development (sandbox) servers; TestFlight and a
/// notarized build to production. This edition installs with Xcode, so tokens
/// are usually sandbox; the app says which when it registers.
const PROD_HOST: &str = "https://api.push.apple.com";
const SANDBOX_HOST: &str = "https://api.sandbox.push.apple.com";

/// A provider token is good for up to an hour and must not be remade more than
/// once every 20 minutes (Apple rejects a storm of new tokens). Kept and reused
/// until this old.
const TOKEN_TTL_SECS: i64 = 40 * 60;

/// How Apple answered: delivered, the token is gone (forget it), a refusal
/// that won't change (drop it), or try again later.
#[derive(Debug)]
pub enum Delivery {
    Sent,
    /// The token is no longer valid for this app: remove it.
    Gone,
    /// Apple refused for a reason retrying won't fix (bad payload, bad topic).
    Refused(String),
    /// A transient failure (429, 5xx, network): retry after the hint, or with
    /// the effects queue's own backoff when there's none.
    Retry(Option<Duration>, String),
}

/// Your APNs auth key and the ids that name it. One key signs every push; the
/// app's bundle id (the topic) and the sandbox/production choice come from the
/// device registration, so one key serves both apps.
pub struct Apns {
    key: EcdsaKeyPair,
    key_id: String,
    team_id: String,
    /// The current provider token and when it was made.
    token: Mutex<Option<(String, i64)>>,
}

impl Apns {
    /// Built from config when APNS_AUTH_KEY, APNS_KEY_ID and APNS_TEAM_ID are
    /// all set (notifications to the apps are on); None otherwise.
    pub fn from_config(cfg: &crate::config::Config) -> Result<Option<Self>> {
        if cfg.apns_auth_key.is_empty() {
            return Ok(None);
        }
        if cfg.apns_key_id.is_empty() || cfg.apns_team_id.is_empty() {
            bail!("APNS_AUTH_KEY is set but APNS_KEY_ID or APNS_TEAM_ID is missing");
        }
        let pkcs8 = pkcs8_from_key(&cfg.apns_auth_key).context("APNS_AUTH_KEY")?;
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8)
            .map_err(|err| anyhow!("APNS_AUTH_KEY isn't a valid .p8 signing key: {err}"))?;
        Ok(Some(Self {
            key,
            key_id: cfg.apns_key_id.clone(),
            team_id: cfg.apns_team_id.clone(),
            token: Mutex::new(None),
        }))
    }

    /// A provider token (a signed JWT), reused until it's TOKEN_TTL_SECS old.
    fn provider_token(&self, now: i64) -> Result<String> {
        let mut held = self.token.lock().unwrap();
        if let Some((token, made)) = held.as_ref()
            && now - made < TOKEN_TTL_SECS
        {
            return Ok(token.clone());
        }
        let header = b64(json!({"alg": "ES256", "kid": self.key_id}).to_string().as_bytes());
        let claims = b64(json!({"iss": self.team_id, "iat": now}).to_string().as_bytes());
        let signed = format!("{header}.{claims}");
        let signature = self
            .key
            .sign(&lc_rand::SystemRandom::new(), signed.as_bytes())
            .map_err(|_| anyhow!("signing the APNs token"))?;
        let token = format!("{signed}.{}", b64(signature.as_ref()));
        *held = Some((token.clone(), now));
        Ok(token)
    }

    /// Drops the cached provider token, so the next send re-signs. Called when
    /// Apple says the token expired: without this, a retry would re-send the
    /// same rejected token (it's good for 40 minutes here) and never get
    /// through.
    fn invalidate_token(&self) {
        *self.token.lock().unwrap() = None;
    }

    /// Sends one notification to one device. `message` is the APNs payload
    /// (the `aps` dictionary and any custom keys); `topic` is the app's bundle
    /// id; `production` picks the APNs host.
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        client: &reqwest::Client,
        token: &str,
        topic: &str,
        production: bool,
        message: &Value,
        collapse_id: &str,
        priority: &str,
        expiration: i64,
        now: i64,
    ) -> Delivery {
        let jwt = match self.provider_token(now) {
            Ok(jwt) => jwt,
            Err(err) => return Delivery::Retry(None, format!("{err:#}")),
        };
        let host = if production { PROD_HOST } else { SANDBOX_HOST };
        let url = format!("{host}/3/device/{token}");
        let mut request = client
            .post(&url)
            .bearer_auth(&jwt)
            .header("apns-topic", topic)
            .header("apns-push-type", "alert")
            .header("apns-priority", priority)
            .header("apns-expiration", expiration.to_string())
            .json(message);
        // The collapse id replaces an unshown notification with the same id
        // (a task's newer card supersedes its older one). Apple's limit is 64
        // bytes; truncate at a char boundary so any input is safe.
        if !collapse_id.is_empty() {
            request = request.header("apns-collapse-id", truncate_bytes(collapse_id, 64));
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => return Delivery::Retry(None, format!("reaching Apple: {err}")),
        };
        let status = response.status().as_u16();
        if status == 200 {
            return Delivery::Sent;
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let reason = response
            .json::<Value>()
            .await
            .ok()
            .and_then(|body| body["reason"].as_str().map(str::to_string))
            .unwrap_or_default();
        // The cached token Apple just rejected is no good: re-sign next time.
        if status == 403 && reason == "ExpiredProviderToken" {
            self.invalidate_token();
        }
        classify(status, &reason, retry_after)
    }
}

/// `text` cut to at most `max` bytes, at a UTF-8 char boundary (never panics).
fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Apple's status and `reason` turned into what to do (APNs documentation,
/// "Handling notification responses from APNs").
fn classify(status: u16, reason: &str, retry_after: Option<Duration>) -> Delivery {
    match status {
        // The token isn't valid for this app any more: forget it.
        410 => Delivery::Gone,
        400 if matches!(
            reason,
            "BadDeviceToken" | "DeviceTokenNotForTopic" | "Unregistered"
        ) =>
        {
            Delivery::Gone
        }
        403 if reason == "ExpiredProviderToken" => Delivery::Retry(None, "the provider token expired".into()),
        429 | 500 | 503 => Delivery::Retry(retry_after, format!("Apple is busy ({status} {reason})")),
        _ => Delivery::Refused(format!("{status} {reason}")),
    }
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The PKCS#8 DER inside an Apple `.p8` key. Accepts the key as its PEM text,
/// or that text base64-encoded (how the deploy stores it, so a multi-line key
/// fits one environment variable).
fn pkcs8_from_key(value: &str) -> Result<Vec<u8>> {
    let text = value.trim();
    let pem = if text.contains("-----BEGIN") {
        text.to_string()
    } else {
        String::from_utf8(STANDARD.decode(text.replace(['\n', '\r', ' '], ""))?)
            .context("not base64 of a .p8 key")?
    };
    let der: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<String>()
        .replace(['\n', '\r', ' '], "");
    if der.is_empty() {
        bail!("empty key");
    }
    STANDARD.decode(der).context("the .p8's base64 body")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway P-256 key as a `.p8` PEM, for the tests.
    fn test_p8() -> String {
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
        let der = key.to_pkcs8v1().unwrap();
        let body = STANDARD.encode(der.as_ref());
        let wrapped = body
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        format!("-----BEGIN PRIVATE KEY-----\n{wrapped}\n-----END PRIVATE KEY-----\n")
    }

    #[test]
    fn loads_a_p8_as_pem_or_base64() {
        let pem = test_p8();
        // As the PEM text.
        assert!(pkcs8_from_key(&pem).is_ok());
        // As that text base64-encoded (the deploy's form).
        let encoded = STANDARD.encode(pem.as_bytes());
        assert_eq!(pkcs8_from_key(&encoded).unwrap(), pkcs8_from_key(&pem).unwrap());
        assert!(pkcs8_from_key("not a key").is_err());
    }

    #[test]
    fn signs_and_caches_a_provider_token() {
        let pem = test_p8();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8_from_key(&pem).unwrap())
            .unwrap();
        let apns = Apns {
            key,
            key_id: "ABC123DEFG".into(),
            team_id: "TEAM123456".into(),
            token: Mutex::new(None),
        };
        let token = apns.provider_token(1_000).unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "ABC123DEFG");
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "TEAM123456");
        assert_eq!(claims["iat"], 1_000);
        // Reused until it's TOKEN_TTL_SECS old, then remade.
        assert_eq!(apns.provider_token(1_000 + TOKEN_TTL_SECS - 1).unwrap(), token);
        assert_ne!(apns.provider_token(1_000 + TOKEN_TTL_SECS + 1).unwrap(), token);
    }

    #[test]
    fn an_expired_token_is_dropped_so_the_retry_re_signs() {
        let pem = test_p8();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8_from_key(&pem).unwrap())
            .unwrap();
        let apns = Apns {
            key,
            key_id: "ABC123DEFG".into(),
            team_id: "TEAM123456".into(),
            token: Mutex::new(None),
        };
        let token = apns.provider_token(1_000).unwrap();
        // Normally the same `now` serves the cached token.
        assert_eq!(apns.provider_token(1_000).unwrap(), token);
        // After Apple rejects it as expired, the next call re-signs (a new
        // signature), so the retry doesn't re-send the dead token.
        apns.invalidate_token();
        assert_ne!(apns.provider_token(1_000).unwrap(), token);
    }

    #[test]
    fn truncate_bytes_never_splits_a_char() {
        assert_eq!(truncate_bytes("abcdef", 64), "abcdef");
        assert_eq!(truncate_bytes("abcdef", 3), "abc");
        // "é" is two bytes: cutting at 3 would split it, so it stops at 2.
        assert_eq!(truncate_bytes("aéb", 3), "aé");
        assert_eq!(truncate_bytes("aéb", 2), "a");
        // A 100-character multibyte string truncated to 64 bytes stays valid.
        let long = "é".repeat(100);
        let cut = truncate_bytes(&long, 64);
        assert!(cut.len() <= 64 && long.starts_with(cut));
    }

    #[test]
    fn classifies_apples_answers() {
        assert!(matches!(classify(410, "Unregistered", None), Delivery::Gone));
        assert!(matches!(classify(400, "BadDeviceToken", None), Delivery::Gone));
        assert!(matches!(
            classify(400, "BadCollapseId", None),
            Delivery::Refused(_)
        ));
        assert!(matches!(
            classify(429, "TooManyRequests", None),
            Delivery::Retry(_, _)
        ));
        assert!(matches!(classify(503, "", None), Delivery::Retry(_, _)));
        assert!(matches!(
            classify(403, "InvalidProviderToken", None),
            Delivery::Refused(_)
        ));
    }
}
