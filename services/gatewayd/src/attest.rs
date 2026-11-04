//! Attestation documents from the instance's NitroTPM, through AWS's
//! `nitro-tpm-attest` (installed in the image). Each document carries the
//! image's boot measurements, signed by the Nitro hypervisor, plus what we put
//! in it: the hash of this boot's TLS certificate (`user_data`) and the
//! caller's nonce.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use ring::digest::{SHA256, digest};

const ATTEST: &str = "/usr/bin/nitro-tpm-attest";
/// Memory-backed (/run), private to gatewayd (systemd RuntimeDirectory).
const SCRATCH: &str = "/run/gatewayd";
/// What `user_data` starts with; desktops check it (agentd: attest.rs).
pub const BINDING_PREFIX: &[u8] = b"cloud-desktops/model-gateway/tls-cert-sha256:";

/// The `user_data` binding a document to a TLS certificate: the prefix, then
/// the SHA-256 of the certificate's DER.
pub fn binding(cert_der: &[u8]) -> Vec<u8> {
    let mut binding = BINDING_PREFIX.to_vec();
    binding.extend_from_slice(digest(&SHA256, cert_der).as_ref());
    binding
}

/// The SHA-256 of the certificate's DER in lowercase hex: what the gateway
/// publishes for desktops that pin it without attesting (agentd: gateway.rs).
pub fn fingerprint(cert_der: &[u8]) -> String {
    digest(&SHA256, cert_der)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A caller's nonce: 16 to 64 bytes, sent as hex.
pub fn parse_nonce(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) || !(32..=128).contains(&hex.len()) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A signed attestation document (COSE_Sign1, CBOR) for this binding and nonce.
pub async fn document(binding: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
    let id = ring_random_hex()?;
    let user_data = format!("{SCRATCH}/attest-{id}.user");
    let nonce_file = format!("{SCRATCH}/attest-{id}.nonce");
    tokio::fs::write(&user_data, binding).await?;
    tokio::fs::write(&nonce_file, nonce).await?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(ATTEST)
            .args(["--user-data", &user_data, "--nonce", &nonce_file])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let _ = tokio::fs::remove_file(&user_data).await;
    let _ = tokio::fs::remove_file(&nonce_file).await;
    let output = output
        .context("nitro-tpm-attest timed out")?
        .context("running nitro-tpm-attest")?;
    if !output.status.success() || output.stdout.is_empty() {
        bail!(
            "nitro-tpm-attest failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn ring_random_hex() -> Result<String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 8];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("no randomness"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_the_certificate_and_checks_nonces() {
        let binding = binding(b"certificate");
        assert!(binding.starts_with(BINDING_PREFIX));
        assert_eq!(binding.len(), BINDING_PREFIX.len() + 32);
        assert_eq!(parse_nonce(&"ab".repeat(16)), Some(vec![0xab; 16]));
        assert_eq!(parse_nonce(&"ab".repeat(15)), None); // too short
        assert_eq!(parse_nonce(&"ab".repeat(65)), None); // too long
        assert_eq!(parse_nonce(&"zz".repeat(16)), None);
        assert_eq!(parse_nonce("é"), None);
    }

    #[test]
    fn the_fingerprint_is_the_bound_hash_in_hex() {
        let fingerprint = fingerprint(b"certificate");
        assert_eq!(fingerprint.len(), 64);
        assert!(
            fingerprint
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        let bound: String = binding(b"certificate")[BINDING_PREFIX.len()..]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(fingerprint, bound);
    }
}
