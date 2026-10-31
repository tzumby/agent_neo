//! Checking the model gateway before trusting it with prompts. The gateway
//! boots from an attestable image that nobody can log into; its NitroTPM
//! attestation document, signed by the AWS Nitro hypervisor, names the image
//! (PCR4: boot code, PCR12: kernel command line) and the TLS certificate of
//! this boot (`user_data`). agentd sends prompts only over a connection
//! pinned to that certificate, after every check here passes.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail, ensure};
use aws_lc_rs::signature::{ECDSA_P384_SHA384_ASN1, ECDSA_P384_SHA384_FIXED, UnparsedPublicKey};
use ciborium::Value;
use x509_parser::oid_registry::{OID_KEY_TYPE_EC_PUBLIC_KEY, OID_NIST_EC_P384, OID_SIG_ECDSA_WITH_SHA384};
use x509_parser::prelude::*;

/// AWS's Nitro attestation root, AWS_NitroEnclaves_Root-G1 (SHA-256
/// fingerprint 64:1A:03:21:…:79:BB:5B, as published in the EC2 docs).
pub const NITRO_ROOT: &[u8] = include_bytes!("nitro-root-g1.der");
/// How `user_data` starts (gatewayd: attest.rs); the certificate's SHA-256 follows.
pub const BINDING_PREFIX: &[u8] = b"cloud-desktops/model-gateway/tls-cert-sha256:";
/// A document older (or further ahead) than this isn't accepted.
const MAX_AGE_MS: u64 = 5 * 60 * 1000;
/// COSE algorithm ES384 (ECDSA with P-384 and SHA-384).
const ES384: i128 = -35;

/// One allowed image: PCR index → value.
pub type Measurements = BTreeMap<u8, Vec<u8>>;

/// "4=<hex>,12=<hex>" per image; several images separated by ';'.
pub fn parse_measurements(text: &str) -> Result<Vec<Measurements>> {
    let mut images = Vec::new();
    for image in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let mut pcrs = Measurements::new();
        for pair in image.split(',').map(str::trim) {
            let (index, hex) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("expected <pcr>=<hex>, got {pair}"))?;
            let value = from_hex(hex.trim()).ok_or_else(|| anyhow!("PCR{index} isn't hex"))?;
            ensure!(value.len() == 48, "PCR{index} isn't a SHA-384 value");
            pcrs.insert(index.trim().parse().context("PCR index")?, value);
        }
        images.push(pcrs);
    }
    ensure!(!images.is_empty(), "no allowed gateway measurements");
    Ok(images)
}

pub fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// What a valid document said.
#[derive(Debug)]
pub struct Verified {
    pub user_data: Vec<u8>,
    pub pcrs: BTreeMap<u8, Vec<u8>>,
}

/// Verifies a NitroTPM attestation document (COSE_Sign1, CBOR): its
/// certificate chain from `root`, its ES384 signature, its age and nonce,
/// and that its measurements are one of the `allowed` images.
pub fn verify(
    document: &[u8],
    root: &[u8],
    allowed: &[Measurements],
    nonce: &[u8],
    now_ms: u64,
) -> Result<Verified> {
    let value: Value = ciborium::from_reader(document).context("not CBOR")?;
    let value = match value {
        Value::Tag(18, inner) => *inner,
        other => other,
    };
    let Value::Array(parts) = value else {
        bail!("not a COSE_Sign1 structure")
    };
    let [protected, _unprotected, payload, signature] = parts.as_slice() else {
        bail!("not a COSE_Sign1 structure")
    };
    let (protected, payload, signature) = (bytes(protected)?, bytes(payload)?, bytes(signature)?);
    let header: Value = ciborium::from_reader(protected.as_slice()).context("protected header")?;
    ensure!(
        int_key(&header, 1).and_then(|v| v.as_integer()).map(i128::from) == Some(ES384),
        "not signed with ES384"
    );
    let doc: Value = ciborium::from_reader(payload.as_slice()).context("attestation document")?;

    // The certificate chain: AWS's root, then the bundle's intermediates, then
    // the certificate that signed the document.
    let leaf_der = bytes(text_key(&doc, "certificate")?)?;
    let bundle = match text_key(&doc, "cabundle")? {
        Value::Array(certs) => certs.iter().map(bytes).collect::<Result<Vec<_>>>()?,
        _ => bail!("cabundle isn't a list"),
    };
    ensure!(
        bundle.first().map(Vec::as_slice) == Some(root),
        "doesn't chain to the AWS Nitro root"
    );
    let now_secs = (now_ms / 1000) as i64;
    let mut issuer = certificate(root, now_secs)?;
    for der in bundle[1..].iter() {
        let cert = certificate(der, now_secs)?;
        signed_by(&cert, &issuer)?;
        ensure!(
            cert.basic_constraints()
                .ok()
                .flatten()
                .is_some_and(|bc| bc.value.ca),
            "an intermediate isn't a CA"
        );
        issuer = cert;
    }
    let leaf = certificate(&leaf_der, now_secs)?;
    signed_by(&leaf, &issuer)?;

    // The document's own signature (RFC 9052 Sig_structure).
    let mut signed = Vec::new();
    ciborium::into_writer(
        &Value::Array(vec![
            Value::Text("Signature1".into()),
            Value::Bytes(protected.clone()),
            Value::Bytes(Vec::new()),
            Value::Bytes(payload.clone()),
        ]),
        &mut signed,
    )?;
    UnparsedPublicKey::new(&ECDSA_P384_SHA384_FIXED, p384_key(&leaf)?)
        .verify(&signed, &signature)
        .map_err(|_| anyhow!("the document's signature doesn't verify"))?;

    // What it says.
    ensure!(
        matches!(text_key(&doc, "digest")?, Value::Text(d) if d == "SHA384"),
        "unexpected digest"
    );
    let timestamp = text_key(&doc, "timestamp")?
        .as_integer()
        .and_then(|t| u64::try_from(t).ok())
        .ok_or_else(|| anyhow!("no timestamp"))?;
    ensure!(
        now_ms.abs_diff(timestamp) <= MAX_AGE_MS,
        "the document isn't fresh"
    );
    ensure!(
        bytes(text_key(&doc, "nonce")?).ok().as_deref() == Some(nonce),
        "the document isn't for our nonce"
    );
    let mut pcrs = BTreeMap::new();
    let Value::Map(entries) = text_key(&doc, "nitrotpm_pcrs")? else {
        bail!("no PCRs")
    };
    for (index, value) in entries {
        let index = index
            .as_integer()
            .and_then(|i| u8::try_from(i).ok())
            .ok_or_else(|| anyhow!("bad PCR index"))?;
        pcrs.insert(index, bytes(value)?);
    }
    ensure!(!allowed.is_empty(), "no allowed gateway measurements");
    if !allowed
        .iter()
        .any(|image| image.iter().all(|(index, value)| pcrs.get(index) == Some(value)))
    {
        let show = |i: u8| pcrs.get(&i).map(|v| hex(v)).unwrap_or_default();
        bail!(
            "the gateway isn't running a published image (PCR4 {}, PCR12 {})",
            show(4),
            show(12)
        );
    }
    let user_data = match text_key(&doc, "user_data") {
        Ok(value) => bytes(value).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    Ok(Verified { user_data, pcrs })
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn certificate(der: &[u8], now_secs: i64) -> Result<X509Certificate<'_>> {
    let (_, cert) = parse_x509_certificate(der).map_err(|_| anyhow!("a certificate doesn't parse"))?;
    let now = ASN1Time::from_timestamp(now_secs).map_err(|_| anyhow!("bad time"))?;
    ensure!(
        cert.validity().is_valid_at(now),
        "certificate {} isn't valid now",
        cert.subject()
    );
    Ok(cert)
}

/// The P-384 public key of a certificate, as an uncompressed point.
fn p384_key<'a>(cert: &'a X509Certificate<'_>) -> Result<&'a [u8]> {
    let key = cert.public_key();
    let curve = key.algorithm.parameters.as_ref().and_then(|p| p.as_oid().ok());
    ensure!(
        key.algorithm.algorithm == OID_KEY_TYPE_EC_PUBLIC_KEY && curve == Some(OID_NIST_EC_P384),
        "certificate {} doesn't have a P-384 key",
        cert.subject()
    );
    Ok(&key.subject_public_key.data)
}

fn signed_by(cert: &X509Certificate<'_>, issuer: &X509Certificate<'_>) -> Result<()> {
    ensure!(
        cert.issuer() == issuer.subject(),
        "certificate {} isn't from {}",
        cert.subject(),
        issuer.subject()
    );
    ensure!(
        cert.signature_algorithm.algorithm == OID_SIG_ECDSA_WITH_SHA384,
        "certificate {} isn't signed with ECDSA P-384",
        cert.subject()
    );
    UnparsedPublicKey::new(&ECDSA_P384_SHA384_ASN1, p384_key(issuer)?)
        .verify(cert.tbs_certificate.as_ref(), &cert.signature_value.data)
        .map_err(|_| anyhow!("certificate {}'s signature doesn't verify", cert.subject()))
}

fn bytes(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::Bytes(bytes) => Ok(bytes.clone()),
        _ => bail!("expected bytes"),
    }
}

fn text_key<'a>(map: &'a Value, name: &str) -> Result<&'a Value> {
    let Value::Map(entries) = map else {
        bail!("not a map")
    };
    entries
        .iter()
        .find(|(key, _)| matches!(key, Value::Text(k) if k == name))
        .map(|(_, value)| value)
        .ok_or_else(|| anyhow!("no {name} in the document"))
}

fn int_key(map: &Value, wanted: i128) -> Option<&Value> {
    let Value::Map(entries) = map else { return None };
    entries
        .iter()
        .find(|(key, _)| key.as_integer().map(i128::from) == Some(wanted))
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{ECDSA_P384_SHA384_FIXED_SIGNING, EcdsaKeyPair};
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, PKCS_ECDSA_P384_SHA384,
    };

    const NOW_MS: u64 = 1_793_455_260_000;

    /// A stand-in for AWS's PKI: root → intermediate → signing certificate.
    struct Pki {
        root: Vec<u8>,
        intermediate: Vec<u8>,
        leaf: Vec<u8>,
        leaf_key: EcdsaKeyPair,
    }

    fn new_pki() -> Pki {
        let ca = |name: &str| {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.distinguished_name.push(rcgen::DnType::CommonName, name);
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
        };
        let key = || KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let root = CertifiedIssuer::self_signed(ca("test root"), key()).unwrap();
        let intermediate = CertifiedIssuer::signed_by(ca("test intermediate"), key(), &root).unwrap();
        let leaf_key = key();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "i-0123456789abcdef0");
        let leaf = params.signed_by(&leaf_key, &intermediate).unwrap();
        Pki {
            root: root.der().to_vec(),
            intermediate: intermediate.der().to_vec(),
            leaf: leaf.der().to_vec(),
            leaf_key: EcdsaKeyPair::from_pkcs8(&ECDSA_P384_SHA384_FIXED_SIGNING, &leaf_key.serialize_der())
                .unwrap(),
        }
    }

    fn document(pki: &Pki, pcr4: &[u8], nonce: &[u8], timestamp: u64, tamper: bool) -> Vec<u8> {
        let text = |s: &str| Value::Text(s.into());
        let payload_value = Value::Map(vec![
            (text("module_id"), text("i-0123456789abcdef0-tpm")),
            (text("digest"), text("SHA384")),
            (text("timestamp"), Value::Integer(timestamp.into())),
            (
                text("nitrotpm_pcrs"),
                Value::Map(vec![
                    (Value::Integer(4.into()), Value::Bytes(pcr4.to_vec())),
                    (Value::Integer(12.into()), Value::Bytes(vec![12; 48])),
                ]),
            ),
            (text("certificate"), Value::Bytes(pki.leaf.clone())),
            (
                text("cabundle"),
                Value::Array(vec![
                    Value::Bytes(pki.root.clone()),
                    Value::Bytes(pki.intermediate.clone()),
                ]),
            ),
            (text("public_key"), Value::Null),
            (text("user_data"), Value::Bytes(b"binding".to_vec())),
            (text("nonce"), Value::Bytes(nonce.to_vec())),
        ]);
        let mut payload = Vec::new();
        ciborium::into_writer(&payload_value, &mut payload).unwrap();
        let mut protected = Vec::new();
        ciborium::into_writer(
            &Value::Map(vec![(Value::Integer(1.into()), Value::Integer((-35).into()))]),
            &mut protected,
        )
        .unwrap();
        let mut signed = Vec::new();
        ciborium::into_writer(
            &Value::Array(vec![
                text("Signature1"),
                Value::Bytes(protected.clone()),
                Value::Bytes(vec![]),
                Value::Bytes(payload.clone()),
            ]),
            &mut signed,
        )
        .unwrap();
        let signature = pki.leaf_key.sign(&SystemRandom::new(), &signed).unwrap();
        if tamper {
            let last = payload.len() - 1;
            payload[last] ^= 1;
        }
        let mut out = Vec::new();
        ciborium::into_writer(
            &Value::Tag(
                18,
                Box::new(Value::Array(vec![
                    Value::Bytes(protected),
                    Value::Map(vec![]),
                    Value::Bytes(payload),
                    Value::Bytes(signature.as_ref().to_vec()),
                ])),
            ),
            &mut out,
        )
        .unwrap();
        out
    }

    fn allowed(pcr4: u8) -> Vec<Measurements> {
        vec![Measurements::from([(4, vec![pcr4; 48]), (12, vec![12; 48])])]
    }

    #[test]
    fn accepts_a_genuine_document_and_nothing_else() {
        let pki = new_pki();
        let nonce = [7u8; 32];
        let good = document(&pki, &[4; 48], &nonce, NOW_MS, false);
        let verified = verify(&good, &pki.root, &allowed(4), &nonce, NOW_MS).unwrap();
        assert_eq!(verified.user_data, b"binding");
        assert_eq!(verified.pcrs[&4], vec![4; 48]);

        // Another image, another nonce, an old document, a changed byte.
        let err = |result: Result<Verified>| result.unwrap_err().to_string();
        assert!(err(verify(&good, &pki.root, &allowed(5), &nonce, NOW_MS)).contains("published image"));
        assert!(err(verify(&good, &pki.root, &allowed(4), &[8; 32], NOW_MS)).contains("nonce"));
        assert!(
            err(verify(
                &good,
                &pki.root,
                &allowed(4),
                &nonce,
                NOW_MS + 10 * 60_000
            ))
            .contains("fresh")
        );
        let tampered = document(&pki, &[4; 48], &nonce, NOW_MS, true);
        assert!(verify(&tampered, &pki.root, &allowed(4), &nonce, NOW_MS).is_err());
        // Signed by another PKI (same names), or checked against AWS's real root.
        let other = new_pki();
        let forged = document(&other, &[4; 48], &nonce, NOW_MS, false);
        assert!(err(verify(&forged, &pki.root, &allowed(4), &nonce, NOW_MS)).contains("Nitro root"));
        assert!(err(verify(&good, NITRO_ROOT, &allowed(4), &nonce, NOW_MS)).contains("Nitro root"));
        // A bundle that claims the right root but whose intermediate it didn't sign.
        let mut mixed = new_pki();
        mixed.root = pki.root.clone();
        let mixed = document(&mixed, &[4; 48], &nonce, NOW_MS, false);
        assert!(err(verify(&mixed, &pki.root, &allowed(4), &nonce, NOW_MS)).contains("signature"));
        assert!(err(verify(&good, &pki.root, &[], &nonce, NOW_MS)).contains("no allowed"));
    }

    #[test]
    fn the_embedded_root_is_awss() {
        use aws_lc_rs::digest::{SHA256, digest};
        assert_eq!(
            hex(digest(&SHA256, NITRO_ROOT).as_ref()),
            "641a0321a3e244efe456463195d606317ed7cdcc3c1756e09893f3c68f79bb5b"
        );
    }

    #[test]
    fn parses_measurement_lists() {
        let a = "ab".repeat(48);
        let b = "cd".repeat(48);
        let images = parse_measurements(&format!("4={a},12={b}; 4={b},12={a}")).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0][&4], vec![0xab; 48]);
        assert!(parse_measurements("").is_err());
        assert!(parse_measurements("4=abcd").is_err()); // not SHA-384
        assert!(parse_measurements(&format!("4{a}")).is_err());
    }
}
