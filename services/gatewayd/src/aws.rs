//! The instance's AWS access: metadata and role credentials from IMDSv2, and
//! SigV4-signed calls to AWS JSON APIs (DynamoDB, SSM).

use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

const IMDS: &str = "http://169.254.169.254/latest";
/// Instance-role credentials rotate hourly and last longer; refresh often.
const CREDENTIALS_LIFETIME: Duration = Duration::from_secs(10 * 60);

pub struct Aws {
    imds: reqwest::Client,
    http: reqwest::Client,
    credentials: Mutex<Option<(Credentials, Instant)>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImdsCredentials {
    access_key_id: String,
    secret_access_key: String,
    token: String,
}

impl Aws {
    pub fn new() -> Result<Self> {
        Ok(Self {
            imds: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()?,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            credentials: Mutex::new(None),
        })
    }

    /// `path` under the metadata service's /latest, e.g. meta-data/placement/region.
    pub async fn metadata(&self, path: &str) -> Result<String> {
        let token = self
            .imds
            .put(format!("{IMDS}/api/token"))
            .header("x-aws-ec2-metadata-token-ttl-seconds", "60")
            .send()
            .await
            .context("reaching the instance metadata service")?
            .error_for_status()?
            .text()
            .await?;
        Ok(self
            .imds
            .get(format!("{IMDS}/{path}"))
            .header("x-aws-ec2-metadata-token", token)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("metadata {path}"))?
            .text()
            .await?)
    }

    async fn credentials(&self) -> Result<Credentials> {
        let mut cached = self.credentials.lock().await;
        if let Some((credentials, fetched)) = cached.as_ref()
            && fetched.elapsed() < CREDENTIALS_LIFETIME
        {
            return Ok(credentials.clone());
        }
        let role = self.metadata("meta-data/iam/security-credentials/").await?;
        let role = role.lines().next().unwrap_or_default().trim().to_string();
        let found: ImdsCredentials = serde_json::from_str(
            &self
                .metadata(&format!("meta-data/iam/security-credentials/{role}"))
                .await?,
        )?;
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

    /// A call to an AWS JSON-protocol API, e.g. ("dynamodb",
    /// "DynamoDB_20120810.GetItem"). Returns the JSON answer.
    pub async fn call(&self, service: &str, region: &str, target: &str, body: &Value) -> Result<Value> {
        // DynamoDB speaks JSON 1.0; SSM (and most others) 1.1.
        let content_type = if service == "dynamodb" {
            "application/x-amz-json-1.0"
        } else {
            "application/x-amz-json-1.1"
        };
        let url = format!("https://{service}.{region}.amazonaws.com/");
        let body = serde_json::to_vec(body)?;
        let credentials = self.credentials().await?;
        let identity = credentials.into();
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(region)
            .name(service)
            .time(SystemTime::now())
            .settings(SigningSettings::default())
            .build()?
            .into();
        let headers = [("content-type", content_type), ("x-amz-target", target)];
        let signable = SignableRequest::new("POST", &url, headers.into_iter(), SignableBody::Bytes(&body))?;
        let (instructions, _) = sign(signable, &params)?.into_parts();
        let mut request = self.http.post(&url).body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        for (name, value) in instructions.headers() {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("calling {service}"))?;
        let status = response.status();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!(
                "{target}: {status} {}",
                answer["__type"]
                    .as_str()
                    .or(answer["message"].as_str())
                    .unwrap_or("")
            );
        }
        Ok(answer)
    }
}

/// A plausible AWS region name (it ends up in hostnames).
pub fn valid_region(region: &str) -> Result<()> {
    let ok = !region.is_empty()
        && region.len() <= 32
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then_some(()).ok_or_else(|| anyhow!("not a region: {region}"))
}
