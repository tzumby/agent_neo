//! gatewayd: the model gateway. Desktops send their model calls here; it
//! checks which desktop is calling (its instance role, through STS), keeps the
//! daily budget, adds the provider key and forwards the call to Together.
//!
//! It runs only in its own image: read-only root, no SSH, no SSM, no user
//! scripts, so the provider key never reaches a desktop. Before sending
//! anything, desktops either check its attestation document (on an instance
//! type that can attest) or pin the certificate it publishes to SSM at boot.
//! It never logs or stores prompts or answers, only who called, which model,
//! tokens and cost.

mod attest;
mod aws;
mod config;
mod identity;
mod usage;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tracing_subscriber::EnvFilter;

use crate::aws::Aws;
use crate::config::Config;
use crate::identity::Identities;

/// Where model calls go. Compiled in: changing it changes the image's
/// measurements, which desktops that attest the gateway would refuse.
const PROVIDER: &str = "https://api.together.ai/v1/chat/completions";
const LISTEN: &str = "0.0.0.0:8443";
const MAX_BODY: usize = 8 * 1024 * 1024;

struct Gateway {
    cfg: Config,
    aws: Aws,
    provider: reqwest::Client,
    key: SecretString,
    identities: Identities,
    /// Hash of this boot's TLS certificate, signed into attestation documents.
    binding: Vec<u8>,
    attesting: Semaphore,
}

type App = State<Arc<Gateway>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("gatewayd=info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let aws = Aws::new()?;
    let cfg = Config::load(&aws).await.context("loading the configuration")?;
    let key = provider_key(&aws, &cfg)
        .await
        .context("getting the provider key")?;

    // A fresh TLS key each boot, never written to disk. Desktops pin the
    // certificate that the attestation document names, or the one published.
    let tls = rcgen::generate_simple_self_signed(vec!["model-gateway.cloud-desktops.internal".to_string()])?;
    let cert = tls.cert.der().to_vec();
    let binding = attest::binding(&cert);
    let fingerprint = attest::fingerprint(&cert);
    let rustls =
        axum_server::tls_rustls::RustlsConfig::from_der(vec![cert], tls.signing_key.serialize_der()).await?;

    let gateway = Arc::new(Gateway {
        identities: Identities::new(&cfg)?,
        provider: reqwest::Client::builder()
            .user_agent(concat!("cloud-desktops-gatewayd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(180))
            .build()?,
        key: SecretString::from(key.trim().to_string()),
        binding,
        // One at a time: nitro-tpm-attest opens /dev/tpm0, which takes one
        // opener, so a second at the same moment failed ("Device or resource
        // busy"). The others wait their turn (about a second each).
        attesting: Semaphore::new(1),
        aws,
        cfg,
    });
    if !gateway.cfg.certificate_parameter.is_empty() {
        tokio::spawn(publish_certificate(gateway.clone(), fingerprint));
    }
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/attestation", get(attestation))
        .route("/v1/chat/completions", post(chat))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(gateway);
    // GATEWAYD_LISTEN: development only (the image's unit doesn't set it).
    let address: SocketAddr = std::env::var("GATEWAYD_LISTEN")
        .unwrap_or(LISTEN.into())
        .parse()?;
    tracing::info!(%address, version = env!("CARGO_PKG_VERSION"), "listening");
    axum_server::bind_rustls(address, rustls)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}

/// Publishes this boot's certificate fingerprint, for desktops that pin it
/// instead of attesting (agentd: GATEWAY_CERTIFICATE_PARAMETER). Until it's
/// there, they refuse this gateway, so it retries until SSM takes it.
async fn publish_certificate(gateway: Arc<Gateway>, fingerprint: String) {
    let parameter = &gateway.cfg.certificate_parameter;
    let mut wait = Duration::from_secs(2);
    loop {
        let put = gateway
            .aws
            .call(
                "ssm",
                &gateway.cfg.region,
                "AmazonSSM.PutParameter",
                &json!({"Name": parameter, "Value": fingerprint, "Type": "String", "Overwrite": true}),
            )
            .await;
        match put {
            Ok(_) => {
                tracing::info!(%parameter, certificate = %&fingerprint[..16], "published the certificate");
                return;
            }
            Err(err) => {
                tracing::warn!(%parameter, error = %err, "can't publish the certificate yet");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// The provider's API key, from SSM, held in memory only. GATEWAYD_KEY_FILE
/// replaces it in development (the image's unit doesn't set it).
async fn provider_key(aws: &Aws, cfg: &Config) -> Result<String> {
    if let Ok(path) = std::env::var("GATEWAYD_KEY_FILE") {
        return Ok(std::fs::read_to_string(path)?);
    }
    let answer = aws
        .call(
            "ssm",
            &cfg.region,
            "AmazonSSM.GetParameter",
            &json!({"Name": cfg.key_parameter, "WithDecryption": true}),
        )
        .await?;
    answer["Parameter"]["Value"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("{} has no value", cfg.key_parameter))
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

#[derive(Deserialize)]
struct Nonce {
    nonce: String,
}

/// The NitroTPM attestation document for this boot: the image's measurements,
/// the caller's nonce, and the hash of the TLS certificate it's talking to.
async fn attestation(State(gateway): App, Query(query): Query<Nonce>) -> Response {
    let Some(nonce) = attest::parse_nonce(&query.nonce) else {
        return error(StatusCode::BAD_REQUEST, "nonce: 16 to 64 bytes, in hex");
    };
    let Ok(_permit) = gateway.attesting.acquire().await else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "shutting down");
    };
    match attest::document(&gateway.binding, &nonce).await {
        Ok(document) => ([(header::CONTENT_TYPE, "application/cbor")], document).into_response(),
        Err(err) => {
            tracing::error!(%err, "couldn't get an attestation document");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "couldn't get an attestation document",
            )
        }
    }
}

async fn chat(State(gateway): App, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    let desktop = match gateway.identities.desktop(&headers).await {
        Ok(desktop) => desktop,
        Err(err) => {
            tracing::info!(reason = %err, "rejected a caller");
            return error(StatusCode::FORBIDDEN, "only desktops can use the model gateway");
        }
    };
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "body must be JSON");
    };
    let prices = match usage::check(&request, &gateway.cfg.prices) {
        Ok(prices) => prices,
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    let model = request["model"].as_str().unwrap_or_default().to_string();
    let day = usage::day();
    match usage::spent(&gateway.aws, &gateway.cfg, &desktop, &day).await {
        Ok(spent) if spent >= gateway.cfg.daily_budget_usd => {
            tracing::info!(desktop, spent, "over budget");
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "this desktop's daily model budget is used up",
            );
        }
        Ok(_) => {}
        Err(err) => {
            tracing::error!(%err, "couldn't read usage");
            return error(StatusCode::SERVICE_UNAVAILABLE, "usage records unavailable");
        }
    }

    let upstream = gateway
        .provider
        .post(PROVIDER)
        .bearer_auth(gateway.key.expose_secret())
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await;
    let upstream = match upstream {
        Ok(response) => response,
        Err(err) => {
            // The error names the URL and a kind, never the body.
            tracing::warn!(desktop, error = %err.without_url(), "provider unreachable");
            return error(StatusCode::BAD_GATEWAY, "the model provider is unreachable");
        }
    };
    let status = upstream.status();
    let retry_after = upstream.headers().get(header::RETRY_AFTER).cloned();
    let answer = match upstream.bytes().await {
        Ok(answer) => answer,
        Err(_) => {
            return error(StatusCode::BAD_GATEWAY, "the model provider's answer was cut off");
        }
    };
    let (input, output, cost) = if status.is_success() {
        let reply_usage = serde_json::from_slice::<Value>(&answer)
            .ok()
            .map(|reply| reply["usage"].clone())
            .unwrap_or_default();
        let cost = usage::cost(&reply_usage, prices);
        let tokens = (
            reply_usage["prompt_tokens"].as_u64().unwrap_or(0),
            reply_usage["completion_tokens"].as_u64().unwrap_or(0),
        );
        if let Err(err) = usage::record(&gateway.aws, &gateway.cfg, &desktop, &day, cost, tokens).await {
            tracing::error!(%err, "couldn't record usage");
        }
        (tokens.0, tokens.1, cost)
    } else {
        (0, 0, 0.0)
    };
    // Metadata only: never the prompt or the answer.
    tracing::info!(
        desktop,
        model,
        status = status.as_u16(),
        ms = started.elapsed().as_millis() as u64,
        input,
        output,
        cost,
        "chat"
    );
    let mut response = (
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        [(header::CONTENT_TYPE, "application/json")],
        answer,
    )
        .into_response();
    if let Some(value) = retry_after {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}
