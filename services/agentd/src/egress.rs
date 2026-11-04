//! The egress gate: scheduled runs' way out (docs/architecture.md §6.21).
//!
//! During a scheduled run's command the shell user can't reach Squid directly
//! (agentd adds it to the guard firewall's `squid_locked` set), so everything
//! goes through here. Most traffic passes straight on to Squid, with its
//! allowlist and log. Requests to Cloudflare's and Datadog's APIs are opened up
//! instead and carry the user's saved keys, in place of any key the command
//! sent: whatever a run read (logs written by attackers included), it can't
//! send data to someone else's Cloudflare or Datadog account.
//!
//! To look inside those requests, each CONNECT to them is answered with a
//! certificate from a CA made at startup; scheduled commands trust it next to
//! the system's CAs (a bundle written here). The CA's private key never leaves
//! this process. Outside scheduled runs (`pinning` off) everything passes
//! through untouched.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// A saved key's value, by name (Secrets::get).
pub type Keys = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The services whose keys are pinned.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Service {
    Cloudflare,
    Datadog,
}

/// Where the system's CAs are, for the bundle scheduled commands trust.
const SYSTEM_CAS: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
];

/// Not passed on: hop-by-hop headers and every way to authenticate. Datadog's
/// are matched more widely (`datadog_key_header`).
const DROPPED_HEADERS: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "authorization",
    "cookie",
    "x-auth-email",
    "x-auth-key",
    "x-auth-user-service-key",
];

/// Not passed back (the length is the body's own here).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

const MAX_REQUEST_BYTES: usize = 8 << 20;
const MAX_RESPONSE_BYTES: usize = 64 << 20;
/// Hosts with a certificate made, kept for reuse (a few in practice).
const MAX_SITES: usize = 64;

/// Whose keys a host takes: Cloudflare's API, and every Datadog host (the
/// API and the intake hosts, which take a key too).
fn service(host: &str) -> Option<Service> {
    let under = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if host == "api.cloudflare.com" {
        Some(Service::Cloudflare)
    } else if under("datadoghq.com") || under("datadoghq.eu") || under("ddog-gov.com") {
        Some(Service::Datadog)
    } else {
        None
    }
}

/// A header that may carry a Datadog key (DD-API-KEY, DD-APPLICATION-KEY,
/// X-Datadog-…).
fn datadog_key_header(name: &str) -> bool {
    name.contains("api-key") || name.contains("application-key") || name.starts_with("x-datadog")
}

/// The query without keys in it.
fn clean_query(query: Option<&str>) -> String {
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return String::new();
    };
    let kept: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase().replace('_', "-");
            !(name.contains("api-key") || name.contains("application-key"))
        })
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    out.extend_pairs(kept);
    format!("?{}", out.finish())
}

fn text_response(status: StatusCode, text: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(format!("agentd: {text}\n"))));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// A plain HTTP answer on the raw connection (before any TLS).
async fn raw_refusal(stream: &mut (impl AsyncWriteExt + Unpin), text: &str) {
    let body = format!("agentd: {text}\n");
    let head = format!(
        "HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The host of a plain HTTP proxy request's absolute URI.
fn absolute_host(target: &str) -> Option<String> {
    url::Url::parse(target)
        .ok()?
        .host_str()
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
}

/// Certificates valid from last year to two years ahead (made fresh each start).
fn validity(params: &mut CertificateParams) {
    let year = jiff::Zoned::now().year() as i32;
    params.not_before = rcgen::date_time_ymd(year - 1, 1, 1);
    params.not_after = rcgen::date_time_ymd(year + 2, 1, 1);
}

pub struct EgressGate {
    ca: CertifiedIssuer<'static, KeyPair>,
    keys: Keys,
    /// Squid, host:port: where everything goes on to.
    squid: String,
    client: reqwest::Client,
    /// Tests and the dev stack: opened-up requests go to `{upstream}/{host}{path}`
    /// (plain HTTP) instead of the real hosts.
    upstream: Option<String>,
    sites: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
    /// On while a scheduled run's command runs: Cloudflare's and Datadog's
    /// keys are pinned then.
    pinning: AtomicBool,
}

impl EgressGate {
    pub fn new(keys: Keys, squid: &str, upstream: Option<String>) -> Result<Self> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(DnType::CommonName, "agentd egress gate for scheduled runs");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        validity(&mut params);
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate()?)?;
        let mut client = reqwest::Client::builder()
            .user_agent(concat!("agentd/", env!("CARGO_PKG_VERSION")))
            // A redirect goes back to the command, which asks again.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(300));
        // Out through Squid like everything else: its allowlist and its log.
        client = if upstream.is_some() || squid.is_empty() {
            client.no_proxy()
        } else {
            client.proxy(reqwest::Proxy::all(format!("http://{squid}"))?)
        };
        Ok(Self {
            ca,
            keys,
            squid: squid.to_string(),
            client: client.build()?,
            upstream,
            sites: Mutex::default(),
            pinning: AtomicBool::new(false),
        })
    }

    pub fn set_pinning(&self, on: bool) {
        self.pinning.store(on, Ordering::SeqCst);
    }

    fn pinning(&self) -> bool {
        self.pinning.load(Ordering::SeqCst)
    }

    /// The CA certificate (PEM).
    pub fn ca_pem(&self) -> String {
        self.ca.pem()
    }

    /// Writes the bundle scheduled commands trust: the system's CAs and ours.
    pub fn write_bundle(&self, path: &Path) -> Result<()> {
        let system = SYSTEM_CAS
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let bundle = format!("{}\n{}", system.trim_end(), self.ca_pem());
        std::fs::write(path, bundle).with_context(|| format!("writing {}", path.display()))
    }

    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let gate = self.clone();
            tokio::spawn(async move {
                // A command runs 15 minutes at most.
                let _ = tokio::time::timeout(Duration::from_secs(960), gate.connection(stream)).await;
            });
        }
    }

    async fn connection(self: Arc<Self>, stream: TcpStream) {
        let mut stream = BufReader::new(stream);
        // The request head, kept as it came for Squid.
        let mut head = String::new();
        let read_head = async {
            loop {
                let n = stream.read_line(&mut head).await.ok()?;
                if n == 0 || head.len() > 16384 {
                    return None;
                }
                if head.ends_with("\r\n\r\n") || head.ends_with("\n\n") || head == "\r\n" {
                    return Some(());
                }
            }
        };
        if tokio::time::timeout(Duration::from_secs(10), read_head)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            return;
        }
        let mut words = head.split_whitespace();
        let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
        let pinning = self.pinning();
        if method == "CONNECT" {
            let (host, port) = target.rsplit_once(':').unwrap_or((target, "443"));
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            if pinning && port == "443" && service(&host).is_some() {
                self.open_up(stream, host).await;
                return;
            }
        } else if pinning && absolute_host(target).is_some_and(|h| service(&h).is_some()) {
            tracing::info!("egress gate: refused plain HTTP to a pinned host");
            raw_refusal(
                &mut stream,
                "Cloudflare's and Datadog's APIs are reached over https only",
            )
            .await;
            return;
        }
        self.pass_on(stream, &head).await;
    }

    /// Straight on to Squid, which answers as it would have.
    async fn pass_on(&self, stream: BufReader<TcpStream>, head: &str) {
        let Ok(mut squid) = TcpStream::connect(&self.squid).await else {
            let mut stream = stream;
            raw_refusal(&mut stream, "the egress proxy isn't answering").await;
            return;
        };
        let buffered = stream.buffer().to_vec();
        let mut client = stream.into_inner();
        if squid.write_all(head.as_bytes()).await.is_err() || squid.write_all(&buffered).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut squid).await;
    }

    /// A pinned host: TLS here, each request sent on with the saved keys.
    async fn open_up(self: Arc<Self>, mut stream: BufReader<TcpStream>, host: String) {
        let config = match self.site(&host) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(%err, "egress gate: couldn't make a certificate");
                raw_refusal(&mut stream, "couldn't set up the connection").await;
                return;
            }
        };
        if stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        let Ok(tls) = tokio_rustls::TlsAcceptor::from(config).accept(stream).await else {
            return;
        };
        let gate = self.clone();
        let service = hyper::service::service_fn(move |request| {
            let gate = gate.clone();
            let host = host.clone();
            async move { Ok::<_, Infallible>(gate.handle(&host, request).await) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_secs(60))
            .serve_connection(TokioIo::new(tls), service)
            .await;
    }

    /// The TLS setup for a host: a certificate for it from our CA.
    fn site(&self, host: &str) -> Result<Arc<rustls::ServerConfig>> {
        if let Some(config) = self.sites.lock().unwrap().get(host) {
            return Ok(config.clone());
        }
        let mut params = CertificateParams::new(vec![host.to_string()])?;
        params.distinguished_name.push(DnType::CommonName, host);
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        validity(&mut params);
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.ca)?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        let mut sites = self.sites.lock().unwrap();
        if sites.len() >= MAX_SITES {
            sites.clear();
        }
        sites.insert(host.to_string(), config.clone());
        Ok(config)
    }

    /// The saved keys for a service, as the headers it takes.
    fn credentials(&self, service: Service) -> Result<Vec<(&'static str, String)>, String> {
        let missing = |what: &str| {
            format!(
                "no {what} is saved, and scheduled runs reach this service only with the user's own key. Say in \
                 your report that it's missing: the user adds it in the web app's Setup (Keys and tokens)."
            )
        };
        match service {
            Service::Cloudflare => (self.keys)("CLOUDFLARE_API_TOKEN")
                .map(|token| vec![("authorization", format!("Bearer {token}"))])
                .ok_or_else(|| missing("Cloudflare API token (CLOUDFLARE_API_TOKEN)")),
            Service::Datadog => {
                let api = (self.keys)("DD_API_KEY").ok_or_else(|| missing("Datadog API key (DD_API_KEY)"))?;
                let mut headers = vec![("dd-api-key", api)];
                if let Some(app) = (self.keys)("DD_APP_KEY") {
                    headers.push(("dd-application-key", app));
                }
                Ok(headers)
            }
        }
    }

    async fn handle(&self, host: &str, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let (parts, body) = request.into_parts();
        let method = parts.method.clone();
        let Some(service) = service(host) else {
            return text_response(StatusCode::FORBIDDEN, format!("{host} isn't a pinned host"));
        };
        let credentials = match self.credentials(service) {
            Ok(credentials) => credentials,
            Err(why) => return text_response(StatusCode::FORBIDDEN, why),
        };
        let body = match Limited::new(body, MAX_REQUEST_BYTES).collect().await {
            Ok(body) => body.to_bytes(),
            Err(_) => {
                return text_response(StatusCode::PAYLOAD_TOO_LARGE, "the request is over 8 MiB".into());
            }
        };
        let mut path = parts.uri.path().to_string();
        // Datadog's log intake also takes the key in the path.
        if service == Service::Datadog
            && path.starts_with("/v1/input/")
            && let Some((_, key)) = credentials.first()
        {
            path = format!("/v1/input/{key}");
        }
        let path = format!("{path}{}", clean_query(parts.uri.query()));
        let url = match &self.upstream {
            Some(base) => format!("{base}/{host}{path}"),
            None => format!("https://{host}{path}"),
        };
        let mut upstream = self.client.request(method.clone(), url);
        for (name, value) in &parts.headers {
            let name_text = name.as_str();
            let dropped = DROPPED_HEADERS.contains(&name_text)
                || (service == Service::Datadog && datadog_key_header(name_text));
            if !dropped {
                upstream = upstream.header(name, value);
            }
        }
        for (name, value) in credentials {
            let Ok(value) = HeaderValue::from_str(&value) else {
                return text_response(
                    StatusCode::FORBIDDEN,
                    "a saved key has characters a header can't carry; the user saves it again in Setup"
                        .into(),
                );
            };
            upstream = upstream.header(HeaderName::from_static(name), value);
        }
        let mut answer = match upstream.body(body).send().await {
            Ok(answer) => answer,
            Err(err) => {
                tracing::info!(host, %method, "egress gate: upstream failed");
                return text_response(
                    StatusCode::BAD_GATEWAY,
                    format!("couldn't reach {host}: {}", err.without_url()),
                );
            }
        };
        let status = answer.status();
        let headers = answer.headers().clone();
        let mut bytes = Vec::new();
        loop {
            match answer.chunk().await {
                Ok(Some(chunk)) => {
                    bytes.extend_from_slice(&chunk);
                    if bytes.len() > MAX_RESPONSE_BYTES {
                        return text_response(
                            StatusCode::BAD_GATEWAY,
                            "the answer is over 64 MiB: ask for less (a smaller limit or time window)".into(),
                        );
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    return text_response(
                        StatusCode::BAD_GATEWAY,
                        format!("{host} stopped answering: {err}"),
                    );
                }
            }
        }
        tracing::info!(host, %method, status = status.as_u16(), "egress gate: pinned keys");
        let mut response = Response::new(Full::new(Bytes::from(bytes)));
        *response.status_mut() = status;
        for (name, value) in &headers {
            if !HOP_BY_HOP.contains(&name.as_str()) {
                response.headers_mut().append(name, value.clone());
            }
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::AsyncReadExt;

    #[test]
    fn pinned_hosts() {
        assert_eq!(service("api.cloudflare.com"), Some(Service::Cloudflare));
        for host in [
            "api.datadoghq.eu",
            "api.us5.datadoghq.com",
            "http-intake.logs.datadoghq.com",
            "api.ddog-gov.com",
        ] {
            assert_eq!(service(host), Some(Service::Datadog), "{host}");
        }
        for host in [
            "dash.cloudflare.com",
            "github.com",
            "datadoghq.com.evil.com",
            "evildatadoghq.com",
        ] {
            assert_eq!(service(host), None, "{host}");
        }
    }

    #[test]
    fn keys_in_the_query_are_dropped() {
        assert_eq!(clean_query(None), "");
        assert_eq!(clean_query(Some("api_key=x&application_key=y")), "");
        assert_eq!(
            clean_query(Some("per_page=50&DD-API-KEY=x&name=a%20b")),
            "?per_page=50&name=a+b"
        );
    }

    /// The real APIs' stand-in: echoes what reached it.
    async fn upstream() -> (String, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                let header = |name: &str| {
                    request
                        .headers()
                        .get_all(name)
                        .iter()
                        .map(|v| v.to_str().unwrap().to_string())
                        .collect::<Vec<_>>()
                };
                axum::Json(serde_json::json!({
                    "method": request.method().as_str(),
                    "uri": request.uri().to_string(),
                    "authorization": header("authorization"),
                    "dd-api-key": header("dd-api-key"),
                    "dd-application-key": header("dd-application-key"),
                    "x-datadog-api-key": header("x-datadog-api-key"),
                    "x-auth-key": header("x-auth-key"),
                    "cookie": header("cookie"),
                    "accept": header("accept"),
                }))
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, count)
    }

    /// Squid's stand-in: answers every request with what it was sent.
    async fn squid() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let seen = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let body = format!("squid saw: {}", seen.lines().next().unwrap_or(""));
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        address
    }

    async fn gate(keys: &[(&'static str, &'static str)], upstream: &str) -> (String, Arc<EgressGate>) {
        let keys: HashMap<&str, &str> = keys.iter().copied().collect();
        let squid = squid().await;
        let gate = Arc::new(
            EgressGate::new(
                Box::new(move |name| keys.get(name).map(|v| v.to_string())),
                &squid,
                Some(upstream.to_string()),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(gate.clone().serve(listener));
        (address, gate)
    }

    /// A scheduled command: trusts the gate's CA, goes out through it.
    fn client(address: &str, gate: &EgressGate) -> reqwest::Client {
        let mut roots = rustls::RootCertStore::empty();
        let der = rustls::pki_types::pem::PemObject::from_pem_slice(gate.ca_pem().as_bytes()).unwrap();
        roots.add(der).unwrap();
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .proxy(reqwest::Proxy::all(format!("http://{address}")).unwrap())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn pinning_replaces_whatever_key_a_command_sends() {
        let (base, count) = upstream().await;
        let keys = [
            ("CLOUDFLARE_API_TOKEN", "cf-saved"),
            ("DD_API_KEY", "dd-api-saved"),
            ("DD_APP_KEY", "dd-app-saved"),
        ];
        let (address, gate) = gate(&keys, &base).await;
        gate.set_pinning(true);
        let client = client(&address, &gate);

        let echo: serde_json::Value = client
            .get("https://api.cloudflare.com/client/v4/zones?per_page=5")
            .header("Authorization", "Bearer attacker")
            .header("X-Auth-Key", "attacker")
            .header("Cookie", "a=b")
            .header("Accept", "application/json")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(echo["uri"], "/api.cloudflare.com/client/v4/zones?per_page=5");
        assert_eq!(echo["authorization"], serde_json::json!(["Bearer cf-saved"]));
        assert_eq!(echo["x-auth-key"], serde_json::json!([]));
        assert_eq!(echo["cookie"], serde_json::json!([]));
        assert_eq!(echo["accept"], serde_json::json!(["application/json"]));

        // Writes too (to the user's own account, with their key).
        let echo: serde_json::Value = client
            .post("https://http-intake.logs.datadoghq.eu/api/v2/logs?dd-api-key=attacker&x=1")
            .header("DD-API-KEY", "attacker")
            .header("X-Datadog-Api-Key", "attacker")
            .body("[]")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(echo["method"], "POST");
        assert_eq!(echo["uri"], "/http-intake.logs.datadoghq.eu/api/v2/logs?x=1");
        assert_eq!(echo["dd-api-key"], serde_json::json!(["dd-api-saved"]));
        assert_eq!(echo["dd-application-key"], serde_json::json!(["dd-app-saved"]));
        assert_eq!(echo["x-datadog-api-key"], serde_json::json!([]));
        // The key in the legacy intake path is the user's too.
        let echo: serde_json::Value = client
            .post("https://http-intake.logs.datadoghq.com/v1/input/attackerkey")
            .body("x")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            echo["uri"],
            "/http-intake.logs.datadoghq.com/v1/input/dd-api-saved"
        );
        assert_eq!(count.load(Ordering::SeqCst), 3);

        // Other hosts go on to Squid as they came.
        let text = client
            .get("http://example.com/x")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(text, "squid saw: GET http://example.com/x HTTP/1.1");
        let mut raw = TcpStream::connect(&address).await.unwrap();
        raw.write_all(b"CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        raw.read_to_string(&mut answer).await.unwrap();
        assert!(
            answer.ends_with("squid saw: CONNECT github.com:443 HTTP/1.1"),
            "{answer}"
        );
        // Plain HTTP to a pinned host would carry a key in the clear: refused.
        let response = client
            .get("http://api.cloudflare.com/client/v4/zones")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn without_pinning_everything_goes_on_to_squid() {
        let (base, count) = upstream().await;
        let (address, gate) = gate(&[("CLOUDFLARE_API_TOKEN", "cf-saved")], &base).await;
        let mut raw = TcpStream::connect(&address).await.unwrap();
        raw.write_all(b"CONNECT api.cloudflare.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        raw.read_to_string(&mut answer).await.unwrap();
        assert!(
            answer.ends_with("squid saw: CONNECT api.cloudflare.com:443 HTTP/1.1"),
            "{answer}"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(gate);
    }

    #[tokio::test]
    async fn a_missing_key_says_where_to_add_it() {
        let (base, count) = upstream().await;
        let (address, gate) = gate(&[], &base).await;
        gate.set_pinning(true);
        let response = client(&address, &gate)
            .get("https://api.datadoghq.eu/api/v1/validate")
            .header("DD-API-KEY", "attacker")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        let text = response.text().await.unwrap();
        assert!(text.contains("DD_API_KEY") && text.contains("Setup"), "{text}");
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_bundle_holds_the_system_cas_and_ours() {
        let gate = EgressGate::new(Box::new(|_| None), "127.0.0.1:1", None).unwrap();
        let path = std::env::temp_dir().join(format!("agentd-bundle-{}.pem", uuid::Uuid::new_v4().simple()));
        gate.write_bundle(&path).unwrap();
        let bundle = std::fs::read_to_string(&path).unwrap();
        assert!(bundle.ends_with(&gate.ca_pem()));
        assert!(
            bundle.matches("BEGIN CERTIFICATE").count() > 1,
            "the system's CAs are there too"
        );
        std::fs::remove_file(path).unwrap();
    }
}
