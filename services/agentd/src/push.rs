//! Push notifications through the push relay: a Lambda in the AWS account
//! that holds the APNs key. agentd signs its calls with this desktop's
//! instance-role credentials (SigV4), and the relay only reaches devices that
//! were registered through this desktop. The key never comes here.

use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::config::Config;

/// Instance-role credentials rotate hourly and last longer; refresh often.
const CREDENTIALS_LIFETIME: Duration = Duration::from_secs(10 * 60);
const IMDS: &str = "http://169.254.169.254/latest";

/// One notification (the relay checks and trims it again).
#[derive(Clone)]
pub struct Push {
    pub title: String,
    pub body: String,
    /// Notification category in the app (APPROVAL has Approve/Deny).
    pub category: Option<&'static str>,
    /// Groups a task's notifications together.
    pub thread: String,
    /// Short ids for the app: task, approval, action.
    pub data: Value,
}

pub struct Relay {
    url: String,
    region: String,
    client: reqwest::Client,
    imds: reqwest::Client,
    credentials: Mutex<Option<(Credentials, Instant)>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImdsCredentials {
    access_key_id: String,
    secret_access_key: String,
    token: String,
}

impl Relay {
    /// None when no relay is configured (push is off).
    pub fn from_config(cfg: &Config) -> Result<Option<Self>> {
        let url = cfg.push_relay_url.trim_end_matches('/').to_string();
        if url.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            region: region_of(&url)
                .ok_or_else(|| anyhow!("PUSH_RELAY_URL isn't a Lambda function URL: {url}"))?,
            url,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
            imds: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()?,
            credentials: Mutex::new(None),
        }))
    }

    pub async fn register(&self, token: &str, name: &str) -> Result<()> {
        self.post("/devices", &json!({"token": token, "name": name}))
            .await
    }

    pub async fn notify(&self, push: &Push) -> Result<()> {
        let body = json!({
            "title": push.title, "body": push.body, "category": push.category,
            "thread": push.thread, "data": push.data,
        });
        self.post("/notify", &body).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<()> {
        let url = format!("{}{path}", self.url);
        let body = serde_json::to_vec(body)?;
        let headers = signed_headers(
            &self.credentials().await?,
            &self.region,
            &url,
            &body,
            SystemTime::now(),
        )?;
        let mut request = self.client.post(&url).body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.context("calling the push relay")?;
        let status = response.status();
        if !status.is_success() {
            bail!(
                "push relay answered {status}: {}",
                response.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }

    /// The instance role's credentials, from IMDSv2.
    async fn credentials(&self) -> Result<Credentials> {
        let mut cached = self.credentials.lock().await;
        if let Some((credentials, fetched)) = cached.as_ref()
            && fetched.elapsed() < CREDENTIALS_LIFETIME
        {
            return Ok(credentials.clone());
        }
        let token = self
            .imds
            .put(format!("{IMDS}/api/token"))
            .header("x-aws-ec2-metadata-token-ttl-seconds", "300")
            .send()
            .await
            .context("reaching the instance metadata service")?
            .error_for_status()?
            .text()
            .await?;
        let get = |path: String| {
            self.imds
                .get(format!("{IMDS}/meta-data/iam/security-credentials/{path}"))
                .header("x-aws-ec2-metadata-token", &token)
                .send()
        };
        let role = get(String::new()).await?.error_for_status()?.text().await?;
        let role = role.lines().next().unwrap_or_default().trim().to_string();
        let found: ImdsCredentials = get(role).await?.error_for_status()?.json().await?;
        let credentials = Credentials::new(
            found.access_key_id,
            found.secret_access_key,
            Some(found.token),
            None,
            "instance-role",
        );
        *cached = Some((credentials.clone(), Instant::now()));
        Ok(credentials)
    }
}

/// The SigV4 headers (authorization, x-amz-date, x-amz-security-token) for a
/// JSON POST to a Lambda function URL.
fn signed_headers(
    credentials: &Credentials,
    region: &str,
    url: &str,
    body: &[u8],
    time: SystemTime,
) -> Result<Vec<(String, String)>> {
    let identity = credentials.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("lambda")
        .time(time)
        .settings(SigningSettings::default())
        .build()?
        .into();
    let content_type = [("content-type", "application/json")];
    let signable = SignableRequest::new("POST", url, content_type.into_iter(), SignableBody::Bytes(body))?;
    let (instructions, _) = sign(signable, &params)?.into_parts();
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    headers.extend(
        instructions
            .headers()
            .map(|(name, value)| (name.to_string(), value.to_string())),
    );
    Ok(headers)
}

/// "https://abc.lambda-url.ca-central-1.on.aws" → "ca-central-1".
fn region_of(url: &str) -> Option<String> {
    let host = url.strip_prefix("https://")?.split('/').next()?;
    let mut parts = host.split('.');
    let (_, service, region) = (parts.next()?, parts.next()?, parts.next()?);
    (service == "lambda-url" && !region.is_empty()).then(|| region.to_string())
}

/// Cuts text for a notification line.
pub fn clip(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let kept: String = flat.chars().take(max_chars - 1).collect();
    format!("{}…", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_from_function_url() {
        assert_eq!(
            region_of("https://abc123.lambda-url.ca-central-1.on.aws").as_deref(),
            Some("ca-central-1")
        );
        assert_eq!(region_of("https://example.com"), None);
        assert_eq!(region_of("http://abc.lambda-url.ca-central-1.on.aws"), None);
    }

    #[test]
    fn signs_for_lambda_in_the_region() {
        let credentials = Credentials::new("AKIDEXAMPLE", "secret", Some("session".into()), None, "test");
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000); // 2023-11-14
        let headers = signed_headers(
            &credentials,
            "ca-central-1",
            "https://abc.lambda-url.ca-central-1.on.aws/notify",
            br#"{"title":"x"}"#,
            time,
        )
        .unwrap();
        let get = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        let auth = get("authorization").unwrap();
        assert!(
            auth.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20231114/ca-central-1/lambda/aws4_request"
            ),
            "{auth}"
        );
        assert!(
            auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token"),
            "{auth}"
        );
        assert_eq!(get("x-amz-security-token"), Some("session"));
        assert_eq!(get("x-amz-date"), Some("20231114T221320Z"));
    }

    #[test]
    fn clips_long_text() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("one\ntwo   three", 50), "one two three");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
    }
}
