//! The model gateway, from agentd's side. Prompts only go over connections
//! presenting a certificate agentd has pinned, so only that gateway can read
//! them. How agentd learns which certificate (`Trust`):
//! - **Attested:** it fetches an attestation document for a fresh nonce over
//!   TLS, checks it (attest.rs: AWS's signature, a published image, our nonce)
//!   and that it names the certificate this very connection used.
//! - **Published:** it reads the certificate's fingerprint from the SSM
//!   parameter gatewayd writes at boot. That trusts whoever can write the
//!   parameter: the AWS account's admins.
//!
//! Checked again every hour, and when the pinned certificate stops working
//! (the gateway rebooted with a new one).

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, ensure};
use aws_lc_rs::digest::{SHA256, digest};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::sync::Mutex;

use crate::attest::{self, BINDING_PREFIX, Measurements, NITRO_ROOT};
use crate::aws::{InstanceRole, ssm_parameter, sts_proof};

const REPIN_EVERY: Duration = Duration::from_secs(3600);
/// An identity proof is reused this long (STS accepts one for 15 minutes).
const PROOF_FOR: Duration = Duration::from_secs(4 * 60);
const PROOF_HEADER: &str = "x-desktop-identity";

/// How agentd knows which TLS certificate is the real gateway.
#[derive(Clone, Debug)]
pub enum Trust {
    /// Its NitroTPM attestation shows one of these images and names the
    /// certificate (GATEWAY_MEASUREMENTS).
    Attested(Vec<Measurements>),
    /// The fingerprint gatewayd publishes to this SSM parameter at boot
    /// (GATEWAY_CERTIFICATE_PARAMETER).
    Published(String),
}

impl Trust {
    /// From GATEWAY_MEASUREMENTS and GATEWAY_CERTIFICATE_PARAMETER.
    /// Measurements win when both are set: attesting is the stronger check.
    pub fn parse(measurements: &str, certificate_parameter: &str) -> Result<Self> {
        if !measurements.trim().is_empty() {
            return Ok(Self::Attested(
                attest::parse_measurements(measurements).context("GATEWAY_MEASUREMENTS")?,
            ));
        }
        let parameter = certificate_parameter.trim();
        ensure!(
            !parameter.is_empty(),
            "set GATEWAY_MEASUREMENTS (a gateway that attests) or GATEWAY_CERTIFICATE_PARAMETER"
        );
        ensure!(
            parameter.starts_with('/')
                && parameter.len() <= 1011
                && parameter
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/_.-".contains(c)),
            "GATEWAY_CERTIFICATE_PARAMETER isn't an SSM parameter name: {parameter}"
        );
        Ok(Self::Published(parameter.to_string()))
    }
}

pub struct ModelGateway {
    url: String,
    region: String,
    audience: String,
    trust: Trust,
    timeout: Duration,
    role: InstanceRole,
    /// For SSM (Published).
    aws: reqwest::Client,
    pinned: Mutex<Option<(reqwest::Client, Instant)>>,
    proof: Mutex<Option<(String, Instant)>>,
}

impl ModelGateway {
    pub fn new(url: &str, region: &str, audience: &str, trust: Trust, timeout: Duration) -> Result<Self> {
        ensure!(url.starts_with("https://"), "GATEWAY_URL must be https");
        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            region: region.to_string(),
            audience: audience.to_string(),
            trust,
            timeout,
            role: InstanceRole::new()?,
            aws: reqwest::Client::builder()
                .user_agent(concat!("agentd/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(10))
                .build()?,
            pinned: Mutex::new(None),
            proof: Mutex::new(None),
        })
    }

    /// A chat completion through the gateway (the caller checks the status).
    pub async fn chat(&self, body: &[u8]) -> Result<reqwest::Response> {
        let proof = self.proof().await?;
        let mut fresh = false;
        loop {
            let client = self.client(fresh).await?;
            let sent = client
                .post(format!("{}/v1/chat/completions", self.url))
                .header("content-type", "application/json")
                .header(PROOF_HEADER, &proof)
                .body(body.to_vec())
                .send()
                .await;
            match sent {
                Ok(response) => return Ok(response),
                // A refused certificate fails the connection: maybe the gateway
                // restarted with a new one. Pin again, once.
                Err(err) if err.is_connect() && !fresh => {
                    tracing::warn!(error = %err, "can't reach the pinned gateway; pinning again");
                    fresh = true;
                }
                Err(err) => return Err(anyhow!(err).context("calling the model gateway")),
            }
        }
    }

    /// `agentd --check-gateway`: pins the gateway's certificate, then sends a
    /// request it must refuse for its model, not its caller (a 400, not a
    /// 403). That proves the pinned connection and this desktop's identity,
    /// with no prompt and no cost.
    pub async fn check(&self) -> Result<String> {
        let client = self.client(true).await?;
        let response = client
            .post(format!("{}/v1/chat/completions", self.url))
            .header("content-type", "application/json")
            .header(PROOF_HEADER, self.proof().await?)
            .body(r#"{"model":"agentd/check-gateway","messages":[]}"#)
            .send()
            .await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        ensure!(
            status == 400 && body.contains("model not allowed"),
            "the gateway answered {status}: {body}"
        );
        Ok(match &self.trust {
            Trust::Attested(_) => "attested, and this desktop's identity accepted".into(),
            Trust::Published(parameter) => {
                format!("its certificate is the one in {parameter}, and this desktop's identity accepted")
            }
        })
    }

    async fn proof(&self) -> Result<String> {
        let mut cached = self.proof.lock().await;
        if let Some((proof, made)) = cached.as_ref()
            && made.elapsed() < PROOF_FOR
        {
            return Ok(proof.clone());
        }
        let proof = sts_proof(
            &self.role.credentials().await?,
            &self.region,
            &self.audience,
            SystemTime::now(),
        )?;
        *cached = Some((proof.clone(), Instant::now()));
        Ok(proof)
    }

    /// The client pinned to the gateway's certificate, pinning first if needed.
    async fn client(&self, fresh: bool) -> Result<reqwest::Client> {
        let mut pinned = self.pinned.lock().await;
        if !fresh
            && let Some((client, at)) = pinned.as_ref()
            && at.elapsed() < REPIN_EVERY
        {
            return Ok(client.clone());
        }
        let client = match &self.trust {
            Trust::Attested(allowed) => self.attest(allowed).await.context(
                "the model gateway didn't prove it's running the published image, so nothing was sent",
            )?,
            Trust::Published(parameter) => self
                .published(parameter)
                .await
                .context("couldn't read the model gateway's published certificate, so nothing was sent")?,
        };
        *pinned = Some((client.clone(), Instant::now()));
        Ok(client)
    }

    async fn attest(&self, allowed: &[Measurements]) -> Result<reqwest::Client> {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let probe = pinned_client(None, seen.clone(), Duration::from_secs(20))?;
        let mut nonce = [0u8; 32];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow!("no randomness"))?;
        let document = probe
            .get(format!("{}/attestation?nonce={}", self.url, attest::hex(&nonce)))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let certificate = seen
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow!("no certificate seen"))?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let verified = attest::verify(&document, NITRO_ROOT, allowed, &nonce, now_ms)?;
        let mut binding = BINDING_PREFIX.to_vec();
        binding.extend_from_slice(&certificate);
        ensure!(
            verified.user_data == binding,
            "the attestation names a different TLS certificate"
        );
        let pcr4 = verified.pcrs.get(&4).map(|v| attest::hex(v)).unwrap_or_default();
        tracing::info!(pcr4 = %pcr4.get(..16).unwrap_or_default(), "model gateway attested");
        pinned_client(Some(certificate), seen, self.timeout)
    }

    async fn published(&self, parameter: &str) -> Result<reqwest::Client> {
        let value = ssm_parameter(&self.aws, &self.role, &self.region, parameter).await?;
        let certificate = fingerprint(&value)?;
        tracing::info!(
            certificate = %attest::hex(&certificate[..8]),
            "model gateway certificate pinned from SSM"
        );
        pinned_client(Some(certificate), Arc::default(), self.timeout)
    }
}

/// A published fingerprint: the SHA-256 of the certificate's DER, in hex.
/// Anything else (Terraform's placeholder before the gateway's first boot)
/// means there's nothing to pin yet.
fn fingerprint(value: &str) -> Result<[u8; 32]> {
    attest::from_hex(value.trim())
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| anyhow!("the gateway hasn't published its certificate yet"))
}

/// An HTTPS client that accepts only the certificate whose SHA-256 is
/// `expected` (any, when None: only for fetching an attestation document),
/// recording the one it saw. Handshake signatures are still verified, so the
/// server must hold that certificate's key.
fn pinned_client(
    expected: Option<[u8; 32]>,
    seen: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
    timeout: Duration,
) -> Result<reqwest::Client> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = PinnedCert {
        expected,
        seen,
        provider: provider.clone(),
    };
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(concat!("agentd/", env!("CARGO_PKG_VERSION")))
        .no_proxy()
        .timeout(timeout)
        .build()?)
}

#[derive(Debug)]
struct PinnedCert {
    expected: Option<[u8; 32]>,
    seen: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(digest(&SHA256, end_entity.as_ref()).as_ref());
        *self.seen.lock().unwrap() = Some(hash);
        match self.expected {
            Some(expected) if expected != hash => Err(rustls::Error::General(
                "not the model gateway's pinned certificate".into(),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gatewayd's TLS: a self-signed certificate made at boot.
    async fn tls_server() -> (String, [u8; 32], tokio::task::JoinHandle<()>) {
        let tls =
            rcgen::generate_simple_self_signed(vec!["model-gateway.cloud-desktops.internal".to_string()])
                .unwrap();
        let cert = tls.cert.der().clone();
        let mut hash = [0u8; 32];
        hash.copy_from_slice(digest(&SHA256, cert.as_ref()).as_ref());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(tls.signing_key.serialize_der().into());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(tcp).await {
                        let mut buf = [0u8; 4096];
                        let _ = tls.read(&mut buf).await;
                        let _ = tls
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                            .await;
                        let _ = tls.shutdown().await;
                    }
                });
            }
        });
        (url, hash, task)
    }

    #[tokio::test]
    async fn pins_the_attested_certificate_only() {
        let (url, hash, server) = tls_server().await;
        // Probe: any certificate, recorded.
        let seen = Arc::new(std::sync::Mutex::new(None));
        let probe = pinned_client(None, seen.clone(), Duration::from_secs(5)).unwrap();
        assert_eq!(probe.get(&url).send().await.unwrap().text().await.unwrap(), "ok");
        assert_eq!(*seen.lock().unwrap(), Some(hash));
        // Pinned to it: works. Pinned to another certificate: the connection fails
        // before anything is sent.
        let pinned = pinned_client(Some(hash), Arc::default(), Duration::from_secs(5)).unwrap();
        assert!(pinned.get(&url).send().await.unwrap().status().is_success());
        let other = pinned_client(Some([0; 32]), Arc::default(), Duration::from_secs(5)).unwrap();
        let err = other.post(&url).body("a prompt").send().await.unwrap_err();
        assert!(err.is_connect(), "{err:?}");
        server.abort();
    }

    #[tokio::test]
    async fn pins_the_published_fingerprint() {
        let (url, hash, server) = tls_server().await;
        // What gatewayd publishes: the hash in lowercase hex (attest::fingerprint).
        let published = fingerprint(&format!("{}\n", attest::hex(&hash))).unwrap();
        assert_eq!(published, hash);
        let pinned = pinned_client(Some(published), Arc::default(), Duration::from_secs(5)).unwrap();
        assert!(pinned.get(&url).send().await.unwrap().status().is_success());
        // Terraform's placeholder, a truncated or non-hex value: nothing to pin.
        for value in ["pending", "", &attest::hex(&hash[..31]), &"zz".repeat(32)] {
            let err = fingerprint(value).unwrap_err().to_string();
            assert!(err.contains("hasn't published"), "{value}: {err}");
        }
        server.abort();
    }

    #[test]
    fn trust_comes_from_measurements_first_then_the_parameter() {
        let pcr = "ab".repeat(48);
        let measurements = format!("4={pcr},12={pcr}");
        assert!(matches!(
            Trust::parse(&measurements, "/cloud-desktops-shared/dev/gateway/certificate"),
            Ok(Trust::Attested(images)) if images.len() == 1
        ));
        assert!(matches!(
            Trust::parse("", " /cloud-desktops-shared/dev/gateway/certificate "),
            Ok(Trust::Published(p)) if p == "/cloud-desktops-shared/dev/gateway/certificate"
        ));
        // Neither: refuse to start rather than send prompts to an unchecked gateway.
        assert!(Trust::parse("", "").is_err());
        assert!(Trust::parse(" ", " ").is_err());
        // Not a parameter name.
        assert!(Trust::parse("", "cloud-desktops/x").is_err());
        assert!(Trust::parse("", "/x?y=1").is_err());
        // Bad measurements don't fall back to the parameter.
        assert!(Trust::parse("4=zz", "/cloud-desktops-shared/dev/gateway/certificate").is_err());
    }
}
