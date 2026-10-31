//! Which desktop is calling. A desktop proves it with an STS
//! GetCallerIdentity request signed by its instance role (header
//! x-desktop-identity: base64 JSON of the signed headers). The gateway sends
//! that request to STS and reads back the caller's role session: a desktop
//! role's session is named after the instance. The signature covers a header
//! naming this gateway, so a proof can't be replayed against another service.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail, ensure};
use axum::http::HeaderMap;
use base64::Engine;
use ring::digest::{SHA256, digest};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::Config;

pub const PROOF_HEADER: &str = "x-desktop-identity";
/// Signed into each proof, with the gateway's audience value.
pub const AUDIENCE_HEADER: &str = "x-cloud-desktops-audience";
/// The STS request desktops sign (and the gateway sends) byte for byte.
pub const STS_BODY: &str = "Action=GetCallerIdentity&Version=2011-06-15";
pub const STS_CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";
/// A proof's identity is reused this long (STS accepts a signature for 15 min).
const CACHE_FOR: Duration = Duration::from_secs(5 * 60);
const MAX_SKEW_SECS: i64 = 15 * 60;

#[derive(Deserialize)]
struct Proof {
    authorization: String,
    #[serde(rename = "x-amz-date")]
    date: String,
    #[serde(rename = "x-amz-security-token")]
    token: Option<String>,
    audience: String,
}

pub struct Identities {
    sts: reqwest::Client,
    url: String,
    region: String,
    account_id: String,
    prefix: String,
    audience: String,
    cache: Mutex<HashMap<Vec<u8>, (String, Instant)>>,
}

impl Identities {
    pub fn new(cfg: &Config) -> Result<Self> {
        Ok(Self {
            sts: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            url: format!("https://sts.{}.amazonaws.com/", cfg.region),
            region: cfg.region.clone(),
            account_id: cfg.account_id.clone(),
            prefix: cfg.desktop_role_prefix.clone(),
            audience: cfg.audience.clone(),
            cache: Mutex::default(),
        })
    }

    /// The calling desktop's instance ID, or why it isn't one.
    pub async fn desktop(&self, headers: &HeaderMap) -> Result<String> {
        let raw = headers
            .get(PROOF_HEADER)
            .ok_or_else(|| anyhow!("no identity proof"))?
            .as_bytes();
        let proof: Proof = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(raw)?)?;
        check_proof(&proof, &self.region, &self.audience, unix_now())?;
        let key = digest(&SHA256, proof.authorization.as_bytes()).as_ref().to_vec();
        {
            let mut cache = self.cache.lock().await;
            cache.retain(|_, (_, at)| at.elapsed() < CACHE_FOR);
            if let Some((desktop, _)) = cache.get(&key) {
                return Ok(desktop.clone());
            }
        }
        let mut request = self
            .sts
            .post(&self.url)
            .header("content-type", STS_CONTENT_TYPE)
            .header("x-amz-date", &proof.date)
            .header(AUDIENCE_HEADER, &proof.audience)
            .header("authorization", &proof.authorization)
            .body(STS_BODY);
        if let Some(token) = &proof.token {
            request = request.header("x-amz-security-token", token);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        ensure!(status.is_success(), "STS refused the proof ({status})");
        let arn = between(&text, "<Arn>", "</Arn>").ok_or_else(|| anyhow!("STS answered without an ARN"))?;
        let desktop = caller_desktop(arn, &self.account_id, &self.prefix)
            .ok_or_else(|| anyhow!("not a desktop: {arn}"))?;
        let mut cache = self.cache.lock().await;
        if cache.len() > 10_000 {
            cache.clear();
        }
        cache.insert(key, (desktop.clone(), Instant::now()));
        Ok(desktop)
    }
}

/// Checks a proof before asking STS: signed for STS in our region, recent,
/// and for this gateway (the audience header is signed and has our value).
fn check_proof(proof: &Proof, region: &str, audience: &str, now: i64) -> Result<()> {
    ensure!(proof.audience == audience, "proof made for another service");
    let auth = &proof.authorization;
    ensure!(auth.starts_with("AWS4-HMAC-SHA256 "), "not a SigV4 signature");
    let field = |name: &str| {
        auth.split([' ', ','])
            .find_map(|part| part.trim().strip_prefix(name))
            .ok_or_else(|| anyhow!("signature without {name}"))
    };
    let scope: Vec<&str> = field("Credential=")?.split('/').collect();
    ensure!(
        scope.len() == 5 && scope[2] == region && scope[3] == "sts" && scope[4] == "aws4_request",
        "not signed for STS in {region}"
    );
    let signed: Vec<&str> = field("SignedHeaders=")?.split(';').collect();
    for header in ["host", "x-amz-date", AUDIENCE_HEADER] {
        ensure!(signed.contains(&header), "{header} isn't signed");
    }
    let age = now - parse_amz_date(&proof.date)?;
    ensure!(age.abs() <= MAX_SKEW_SECS, "proof is too old");
    Ok(())
}

/// The instance ID when `arn` is a desktop role's instance session in our
/// account: arn:aws:sts::<account>:assumed-role/<prefix>[-<user>]/i-….
pub fn caller_desktop(arn: &str, account_id: &str, prefix: &str) -> Option<String> {
    let rest = arn.strip_prefix(&format!("arn:aws:sts::{account_id}:assumed-role/"))?;
    let (role, session) = rest.split_once('/')?;
    let role_ok = role == prefix
        || role
            .strip_prefix(prefix)
            .and_then(|r| r.strip_prefix('-'))
            .is_some_and(|user| {
                !user.is_empty()
                    && user
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "+=,.@_-".contains(c))
            });
    let session_ok = session.len() >= 10
        && session.len() <= 19
        && session.strip_prefix("i-").is_some_and(|hex| {
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        });
    (role_ok && session_ok).then(|| session.to_string())
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let to = from + text[from..].find(end)?;
    Some(&text[from..to])
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// "20261031T175804Z" → Unix seconds.
fn parse_amz_date(date: &str) -> Result<i64> {
    let digits = |range: std::ops::Range<usize>| -> Result<i64> {
        date.get(range)
            .filter(|s| s.chars().all(|c| c.is_ascii_digit()))
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("bad x-amz-date"))
    };
    if date.len() != 16 || &date[8..9] != "T" || !date.ends_with('Z') {
        bail!("bad x-amz-date");
    }
    let (y, m, d) = (digits(0..4)?, digits(4..6)?, digits(6..8)?);
    let (hh, mm, ss) = (digits(9..11)?, digits(11..13)?, digits(13..15)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "461553546091";
    const PREFIX: &str = "cloud-desktops-dev-desktop";

    #[test]
    fn only_desktop_role_sessions_are_desktops() {
        let arn = |rest: &str| format!("arn:aws:sts::{ACCOUNT}:assumed-role/{rest}");
        assert_eq!(
            caller_desktop(
                &arn("cloud-desktops-dev-desktop-tzumby/i-0d4eba96e5b32de07"),
                ACCOUNT,
                PREFIX
            )
            .as_deref(),
            Some("i-0d4eba96e5b32de07")
        );
        assert!(
            caller_desktop(
                &arn("cloud-desktops-dev-desktop/i-0d4eba96e5b32de07"),
                ACCOUNT,
                PREFIX
            )
            .is_some()
        );
        for bad in [
            arn("cloud-desktops-dev-desktopx/i-0d4eba96e5b32de07"), // not the prefix
            arn("cloud-desktops-dev-desktop-/i-0d4eba96e5b32de07"),
            arn("admin/i-0d4eba96e5b32de07"),
            arn("cloud-desktops-dev-desktop/me"), // not an instance session
            arn("cloud-desktops-dev-desktop/i-0D4EBA96E5B32DE07"),
            "arn:aws:sts::111122223333:assumed-role/cloud-desktops-dev-desktop/i-0d4eba96e5b32de07".into(),
            format!("arn:aws:iam::{ACCOUNT}:user/tzumby"),
        ] {
            assert_eq!(caller_desktop(&bad, ACCOUNT, PREFIX), None, "{bad}");
        }
    }

    fn proof(authorization: &str, date: &str, audience: &str) -> Proof {
        Proof {
            authorization: authorization.into(),
            date: date.into(),
            token: Some("token".into()),
            audience: audience.into(),
        }
    }

    #[test]
    fn proofs_must_be_for_sts_here_recent_and_for_us() {
        let now = parse_amz_date("20261031T180000Z").unwrap();
        let good = "AWS4-HMAC-SHA256 Credential=ASIAEXAMPLE/20261031/ca-central-1/sts/aws4_request, \
                    SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-cloud-desktops-audience, \
                    Signature=abc";
        let check = |p: &Proof| check_proof(p, "ca-central-1", "gw", now);
        assert!(check(&proof(good, "20261031T175500Z", "gw")).is_ok());
        assert!(check(&proof(good, "20261031T175500Z", "other")).is_err());
        assert!(check(&proof(good, "20261031T170000Z", "gw")).is_err()); // an hour old
        assert!(
            check(&proof(
                &good.replace("ca-central-1", "us-east-1"),
                "20261031T175500Z",
                "gw"
            ))
            .is_err()
        );
        assert!(check(&proof(&good.replace("/sts/", "/s3/"), "20261031T175500Z", "gw")).is_err());
        assert!(
            check(&proof(
                &good.replace(";x-cloud-desktops-audience", ""),
                "20261031T175500Z",
                "gw"
            ))
            .is_err()
        );
        assert!(check(&proof(good, "yesterday", "gw")).is_err());
    }

    #[test]
    fn parses_amz_dates() {
        assert_eq!(parse_amz_date("19700101T000000Z").unwrap(), 0);
        assert_eq!(parse_amz_date("20000229T000000Z").unwrap(), 951_782_400);
        assert_eq!(parse_amz_date("20261031T140100Z").unwrap(), 1_793_455_260);
    }
}
