//! Web Push (RFC 8030) from agentd itself, to the browsers where you turned on
//! notifications in the web app: no relay, no app store account. The payload is
//! encrypted to the browser (RFC 8291, aes128gcm), so the push service in
//! between (Apple's, Google's, Mozilla's, Microsoft's) carries ciphertext
//! only, and each request is signed with this desktop's VAPID key (RFC 8292),
//! made here on first use and kept in agentd's settings.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use aws_lc_rs::{aead, agreement, hkdf, rand as lc_rand};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::json;
use url::Url;

/// Push services' hosts a subscription may name: Chrome and Edge's (FCM),
/// Safari's, Firefox's, and Windows'. Anything else is refused, so a
/// subscription can't point agentd's requests somewhere else.
const PUSH_HOSTS: &[&str] = &[
    "fcm.googleapis.com",
    ".push.apple.com",
    ".push.services.mozilla.com",
    ".notify.windows.com",
];
/// The record size in the aes128gcm header: one record holds the whole payload.
const RECORD_SIZE: u32 = 4096;
/// Payloads stay well under what every push service accepts (4 KB).
pub const MAX_PAYLOAD: usize = 3000;
/// How long a VAPID token is good for (the services allow up to 24 h).
const TOKEN_SECS: i64 = 12 * 3600;

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url, with or without padding (browsers differ).
pub fn unb64(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .ok()
        .or_else(|| URL_SAFE.decode(text).ok())
}

/// This desktop's VAPID key: it signs every push, and browsers subscribe with
/// its public half (the web app's `applicationServerKey`).
pub struct Vapid {
    key: EcdsaKeyPair,
}

impl Vapid {
    /// A new key, and its PKCS#8 form to keep.
    pub fn generate() -> Result<(Self, Vec<u8>)> {
        let key =
            EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).map_err(|_| anyhow!("VAPID key"))?;
        let pkcs8 = key
            .to_pkcs8v1()
            .map_err(|_| anyhow!("VAPID key"))?
            .as_ref()
            .to_vec();
        Ok((Self { key }, pkcs8))
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8)
            .map_err(|err| anyhow!("VAPID key: {err}"))?;
        Ok(Self { key })
    }

    /// The public key, uncompressed (65 bytes), base64url.
    pub fn public_key(&self) -> String {
        b64(self.key.public_key().as_ref())
    }

    /// The Authorization header for a push to `endpoint`: a JWT for the push
    /// service's origin, signed with this key, and the key itself.
    pub fn authorization(&self, endpoint: &Url, subject: &str, now: i64) -> Result<String> {
        let header = b64(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = b64(json!({
            "aud": endpoint.origin().ascii_serialization(),
            "exp": now + TOKEN_SECS,
            "sub": subject,
        })
        .to_string()
        .as_bytes());
        let signed = format!("{header}.{claims}");
        let signature = self
            .key
            .sign(&lc_rand::SystemRandom::new(), signed.as_bytes())
            .map_err(|_| anyhow!("signing the VAPID token"))?;
        Ok(format!(
            "vapid t={signed}.{}, k={}",
            b64(signature.as_ref()),
            self.public_key()
        ))
    }
}

/// A browser's keys for a subscription: its public key (`p256dh`, 65 bytes)
/// and its auth secret (16 bytes).
#[derive(Clone, Debug)]
pub struct Keys {
    pub p256dh: Vec<u8>,
    pub auth: Vec<u8>,
}

pub fn parse_keys(p256dh: &str, auth: &str) -> Result<Keys, String> {
    let p256dh = unb64(p256dh).ok_or("keys.p256dh isn't base64url")?;
    let auth = unb64(auth).ok_or("keys.auth isn't base64url")?;
    if p256dh.len() != 65 || p256dh[0] != 4 {
        return Err("keys.p256dh isn't an uncompressed P-256 public key".into());
    }
    if auth.len() != 16 {
        return Err("keys.auth must be 16 bytes".into());
    }
    Ok(Keys { p256dh, auth })
}

/// A subscription's endpoint, if it's https on a known push service (or, for
/// local development only, `dev`: http on 127.0.0.1).
pub fn check_endpoint(endpoint: &str, dev: bool) -> Result<Url, String> {
    let url = Url::parse(endpoint.trim()).map_err(|_| "the endpoint isn't a URL".to_string())?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if dev && url.scheme() == "http" && host == "127.0.0.1" {
        return Ok(url);
    }
    let known = PUSH_HOSTS.iter().any(|h| {
        if h.starts_with('.') {
            host.ends_with(h)
        } else {
            host == *h
        }
    });
    if url.scheme() != "https" || !known || url.port().is_some() {
        return Err("the endpoint isn't a known push service's (Chrome, Safari, Firefox or Edge)".into());
    }
    Ok(url)
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf_expand(prk: &hkdf::Prk, info: &[u8], len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0; len];
    prk.expand(&[info], Len(len))
        .and_then(|okm| okm.fill(&mut out))
        .map_err(|_| anyhow!("HKDF"))?;
    Ok(out)
}

/// The keys RFC 8291 derives for one message: content key and nonce.
fn derive(
    ecdh_secret: &[u8],
    auth: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> Result<(Vec<u8>, Vec<u8>)> {
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(ecdh_secret);
    let key_info = [b"WebPush: info\0".as_slice(), ua_public, as_public].concat();
    let ikm = hkdf_expand(&prk_key, &key_info, 32)?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let cek = hkdf_expand(&prk, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = hkdf_expand(&prk, b"Content-Encoding: nonce\0", 12)?;
    Ok((cek, nonce))
}

/// The push message body for `plaintext`, encrypted to the browser's keys.
pub fn encrypt(plaintext: &[u8], keys: &Keys) -> Result<Vec<u8>> {
    let ephemeral =
        agreement::PrivateKey::generate(&agreement::ECDH_P256).map_err(|_| anyhow!("ECDH key"))?;
    let mut salt = [0u8; 16];
    lc_rand::fill(&mut salt).map_err(|_| anyhow!("random salt"))?;
    encrypt_with(plaintext, keys, &ephemeral, &salt)
}

/// `encrypt`, with the server's one-time key and the salt given (tests).
fn encrypt_with(
    plaintext: &[u8],
    keys: &Keys,
    ephemeral: &agreement::PrivateKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>> {
    let as_public = ephemeral.compute_public_key().map_err(|_| anyhow!("ECDH key"))?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &keys.p256dh);
    let ecdh_secret = agreement::agree(
        ephemeral,
        peer,
        anyhow!("ECDH with the browser's key"),
        |secret| Ok(secret.to_vec()),
    )?;
    let (cek, nonce) = derive(&ecdh_secret, &keys.auth, &keys.p256dh, as_public.as_ref(), salt)?;
    // One record: the payload, then the last-record delimiter.
    let mut record = [plaintext, &[2u8]].concat();
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| anyhow!("AES key"))?,
    );
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce).map_err(|_| anyhow!("nonce"))?;
    key.seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut record)
        .map_err(|_| anyhow!("encrypting"))?;
    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + record.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.as_ref().len() as u8);
    body.extend_from_slice(as_public.as_ref());
    body.extend_from_slice(&record);
    Ok(body)
}

/// What the push service did with a message.
#[derive(Debug, PartialEq)]
pub enum Delivery {
    Sent,
    /// The subscription no longer exists (404, 410): forget it.
    Gone,
    /// Refused for good (a bad token, too large): don't try again.
    Refused(String),
    /// Try again later (rate limited, the service down, the network).
    Retry(Option<Duration>, String),
}

pub async fn send(
    http: &reqwest::Client,
    endpoint: &Url,
    body: Vec<u8>,
    authorization: &str,
    urgency: &str,
    ttl: u32,
) -> Delivery {
    let response = http
        .post(endpoint.clone())
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", ttl.to_string())
        .header("Urgency", urgency)
        .header("Authorization", authorization)
        .body(body)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(err) => return Delivery::Retry(None, format!("can't reach the push service: {err}")),
    };
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs);
    match status {
        200..=299 => Delivery::Sent,
        404 | 410 => Delivery::Gone,
        429 => Delivery::Retry(retry_after, "rate limited by the push service".into()),
        500..=599 => Delivery::Retry(retry_after, format!("the push service answered {status}")),
        _ => {
            let detail = response.text().await.unwrap_or_default();
            Delivery::Refused(format!(
                "the push service answered {status}: {}",
                crate::llm::truncate(&detail, 200)
            ))
        }
    }
}

/// The HTTP client for pushes.
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("cloud-desktops-agentd/0.1")
        .build()
        .context("push client")
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    pub fn d(text: &str) -> Vec<u8> {
        unb64(text).unwrap()
    }

    /// The browser's side of RFC 8291, to read what `encrypt` makes.
    pub fn decrypt(body: &[u8], ua_private: &[u8], keys: &Keys) -> Vec<u8> {
        let (salt, rest) = body.split_at(16);
        let (rs, rest) = rest.split_at(4);
        assert_eq!(u32::from_be_bytes(rs.try_into().unwrap()), RECORD_SIZE);
        let (id_len, rest) = rest.split_at(1);
        let (as_public, ciphertext) = rest.split_at(id_len[0] as usize);
        let ua = agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, ua_private).unwrap();
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
        let secret = agreement::agree(&ua, peer, (), |s| Ok(s.to_vec())).unwrap();
        let (cek, nonce) = derive(&secret, &keys.auth, &keys.p256dh, as_public, salt).unwrap();
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
        let mut data = ciphertext.to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(),
                aead::Aad::empty(),
                &mut data,
            )
            .unwrap();
        assert_eq!(plain.last(), Some(&2), "the last record's delimiter");
        plain[..plain.len() - 1].to_vec()
    }

    #[test]
    fn encryption_matches_rfc_8291s_example() {
        // RFC 8291, Appendix A.
        let keys = Keys {
            p256dh: d(
                "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            ),
            auth: d("BTBZMqHH6r4Tts7J_aSIgg"),
        };
        let as_private = agreement::PrivateKey::from_private_key(
            &agreement::ECDH_P256,
            &d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"),
        )
        .unwrap();
        let salt: [u8; 16] = d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let plaintext = d("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        assert_eq!(plaintext, b"When I grow up, I want to be a watermelon");

        // Its intermediate values, so a mismatch says where.
        let as_public = as_private.compute_public_key().unwrap();
        assert_eq!(
            b64(as_public.as_ref()),
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &keys.p256dh);
        let secret = agreement::agree(&as_private, peer, (), |s| Ok(s.to_vec())).unwrap();
        assert_eq!(b64(&secret), "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs");
        let (cek, nonce) = derive(&secret, &keys.auth, &keys.p256dh, as_public.as_ref(), &salt).unwrap();
        assert_eq!(b64(&cek), "oIhVW04MRdy2XN9CiKLxTg");
        assert_eq!(b64(&nonce), "4h_95klXJ5E_qnoN");

        let body = encrypt_with(&plaintext, &keys, &as_private, &salt).unwrap();
        let header = d(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
        );
        let ciphertext = d("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ");
        assert_eq!(body, [header, ciphertext].concat());

        // And the browser reads it back.
        assert_eq!(
            decrypt(&body, &d("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94"), &keys),
            plaintext
        );
    }

    #[test]
    fn a_fresh_message_is_one_the_browser_can_read() {
        // The browser's keys from RFC 8291; agentd's side is new each time.
        let keys = Keys {
            p256dh: d(
                "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            ),
            auth: vec![7; 16],
        };
        let message = br#"{"title":"Needs your OK","body":"Clear the old build"}"#;
        let first = encrypt(message, &keys).unwrap();
        let second = encrypt(message, &keys).unwrap();
        assert_ne!(first, second, "a new key and salt each time");
        assert_eq!(
            decrypt(&first, &d("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94"), &keys),
            message
        );
    }

    #[test]
    fn the_vapid_token_is_signed_for_the_push_services_origin() {
        let (vapid, pkcs8) = Vapid::generate().unwrap();
        let again = Vapid::from_pkcs8(&pkcs8).unwrap();
        assert_eq!(vapid.public_key(), again.public_key());
        assert_eq!(d(&vapid.public_key()).len(), 65);
        let endpoint = Url::parse("https://fcm.googleapis.com/fcm/send/abc:def").unwrap();
        let header = again
            .authorization(&endpoint, "mailto:me@example.com", 1_000)
            .unwrap();
        let (token, key) = header
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(key, vapid.public_key());
        let parts: Vec<&str> = token.split('.').collect();
        let claims: serde_json::Value = serde_json::from_slice(&d(parts[1])).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "mailto:me@example.com");
        assert_eq!(claims["exp"], 1_000 + TOKEN_SECS);
        let signed = format!("{}.{}", parts[0], parts[1]);
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, d(key))
            .verify(signed.as_bytes(), &d(parts[2]))
            .expect("a valid ES256 signature");
    }

    #[test]
    fn only_push_services_endpoints_are_taken() {
        for good in [
            "https://fcm.googleapis.com/fcm/send/abc",
            "https://web.push.apple.com/QH4…",
            "https://updates.push.services.mozilla.com/wpush/v2/gAAA",
            "https://wns2-par02p.notify.windows.com/w/?token=x",
        ] {
            assert!(check_endpoint(good, false).is_ok(), "{good}");
        }
        for bad in [
            "http://fcm.googleapis.com/fcm/send/abc",
            "https://evil.example.com/push",
            "https://fcm.googleapis.com.evil.com/x",
            "https://fcm.googleapis.com:8443/x",
            "https://169.254.169.254/latest",
            "http://127.0.0.1:8091/push/x",
            "not a url",
        ] {
            assert!(check_endpoint(bad, false).is_err(), "{bad}");
        }
        assert!(check_endpoint("http://127.0.0.1:8091/push/x", true).is_ok());
        assert!(check_endpoint("http://10.0.0.1:8091/push/x", true).is_err());
    }

    #[test]
    fn browser_keys_are_checked() {
        let p256dh =
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
        assert!(parse_keys(p256dh, "BTBZMqHH6r4Tts7J_aSIgg").is_ok());
        assert!(
            parse_keys(&format!("{p256dh}="), "BTBZMqHH6r4Tts7J_aSIgg==").is_ok(),
            "padded"
        );
        assert!(parse_keys("AAAA", "BTBZMqHH6r4Tts7J_aSIgg").is_err());
        assert!(parse_keys(p256dh, "AAAA").is_err());
    }
}
