//! Calls to our Lambda function URLs (the push relay, the model gateway) as
//! this desktop: SigV4-signed with the instance role's credentials from the
//! metadata service, so the desktop holds no keys of ours.

use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use serde::Deserialize;
use tokio::sync::Mutex;

/// Instance-role credentials rotate hourly and last longer; refresh often.
const CREDENTIALS_LIFETIME: Duration = Duration::from_secs(10 * 60);
const IMDS: &str = "http://169.254.169.254/latest";

pub struct FunctionUrl {
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

impl FunctionUrl {
    pub fn new(url: &str, timeout: Duration) -> Result<Self> {
        let url = url.trim_end_matches('/').to_string();
        Ok(Self {
            region: region_of(&url).ok_or_else(|| anyhow!("not a Lambda function URL: {url}"))?,
            url,
            client: reqwest::Client::builder()
                .user_agent(concat!("agentd/", env!("CARGO_PKG_VERSION")))
                .timeout(timeout)
                .build()?,
            imds: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()?,
            credentials: Mutex::new(None),
        })
    }

    /// A signed JSON POST to `path`; the caller checks the status.
    pub async fn post(&self, path: &str, body: &[u8]) -> Result<reqwest::Response> {
        let url = format!("{}{path}", self.url);
        let headers = signed_headers(
            &self.credentials().await?,
            &self.region,
            &url,
            body,
            SystemTime::now(),
        )?;
        let mut request = self.client.post(&url).body(body.to_vec());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request.send().await.with_context(|| format!("calling {url}"))
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
}
