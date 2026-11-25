//! Desktop apps from the Snap Store (install_app; docs/desktop-apps.md §10):
//! what snapd says about a snap, read over its socket, which anyone may query.
//! Root installs only through the privileged helper's `install-snap`, which
//! checks the store again; this side builds the card and refuses early.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const SNAPD_SOCKET: &str = "/run/snapd.socket";
/// The publisher checks the store does: the only kinds the helper installs.
const VALIDATED: [&str; 2] = ["verified", "starred"];
const MAX_RESPONSE: u64 = 8 * 1024 * 1024;

/// A snap as the store describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Snap {
    pub name: String,
    pub title: String,
    pub kind: String,
    pub version: String,
    pub channel: String,
    pub confinement: String,
    pub summary: String,
    pub size: Option<u64>,
    pub publisher: String,
    pub publisher_id: String,
    pub validation: String,
}

impl Snap {
    fn from_json(snap: &Value) -> Self {
        let text = |key: &str| snap[key].as_str().unwrap_or_default().to_string();
        let title = Some(text("title"))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| text("name"));
        Self {
            name: text("name"),
            title,
            kind: text("type"),
            version: text("version"),
            channel: text("channel"),
            confinement: text("confinement"),
            summary: text("summary"),
            size: snap["download-size"].as_u64(),
            publisher: snap["publisher"]["display-name"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            publisher_id: snap["publisher"]["id"].as_str().unwrap_or_default().to_string(),
            validation: snap["publisher"]["validation"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        }
    }

    pub fn verified(&self) -> bool {
        VALIDATED.contains(&self.validation.as_str())
    }

    /// Why the helper wouldn't install it, if it wouldn't.
    pub fn refusal(&self) -> Option<String> {
        if self.kind != "app" {
            Some(format!("{} isn't an app (it's a {} snap)", self.name, self.kind))
        } else if self.confinement != "strict" {
            Some(format!(
                "{} isn't strictly confined ({}): it would run outside the Snap sandbox",
                self.name, self.confinement
            ))
        } else if !self.verified() {
            Some(format!(
                "{}'s publisher, {}, isn't verified by the Snap Store",
                self.name, self.publisher
            ))
        } else {
            None
        }
    }
}

/// A store name: lowercase letters, digits and single hyphens, at most 40,
/// with a letter, not starting or ending with a hyphen (snapd's rule).
pub fn valid_name(name: &str) -> bool {
    (1..=40).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.bytes().any(|b| b.is_ascii_lowercase())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// GET from snapd's REST API: its status and JSON body.
async fn get(socket: &str, path: &str) -> Result<(u16, Value)> {
    let exchange = async {
        let mut stream = tokio::net::UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to snapd ({socket})"))?;
        stream
            .write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
            .await?;
        let mut response = Vec::new();
        stream.take(MAX_RESPONSE).read_to_end(&mut response).await?;
        parse_response(&response)
    };
    tokio::time::timeout(Duration::from_secs(60), exchange)
        .await
        .context("snapd didn't answer in time")?
}

fn parse_response(response: &[u8]) -> Result<(u16, Value)> {
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("snapd sent no headers")?;
    let head = String::from_utf8_lossy(&response[..split]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("snapd sent no status")?;
    let body =
        serde_json::from_slice(&response[split + 4..]).context("snapd sent something other than JSON")?;
    Ok((status, body))
}

/// The store's snap with exactly this name, if there is one.
pub async fn find(socket: &str, name: &str) -> Result<Option<Snap>> {
    if !valid_name(name) {
        bail!("not a snap name");
    }
    let (status, body) = get(socket, &format!("/v2/find?name={name}")).await?;
    if status != 200 {
        return Ok(None);
    }
    Ok(body["result"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|snap| snap["name"] == name)
        .map(Snap::from_json))
}

/// Snaps the store finds for words, for when a name isn't one.
pub async fn search(socket: &str, words: &str) -> Result<Vec<Snap>> {
    let query: String = words
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-')
        .take(60)
        .collect::<String>()
        .trim()
        .replace(' ', "+");
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let (status, body) = get(socket, &format!("/v2/find?q={query}")).await?;
    if status != 200 {
        return Ok(Vec::new());
    }
    Ok(body["result"]
        .as_array()
        .into_iter()
        .flatten()
        .take(8)
        .map(Snap::from_json)
        .collect())
}

/// The installed snap's version, if it's installed.
pub async fn installed(socket: &str, name: &str) -> Result<Option<String>> {
    if !valid_name(name) {
        return Ok(None);
    }
    let (status, body) = get(socket, &format!("/v2/snaps/{name}")).await?;
    Ok((status == 200).then(|| body["result"]["version"].as_str().unwrap_or_default().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn slack() -> Value {
        json!({"name": "slack", "title": "Slack", "type": "app", "version": "4.53.292", "channel": "stable",
               "confinement": "strict", "summary": "Team communication for the 21st century.",
               "download-size": 233066496u64,
               "publisher": {"id": "GKq9csPRQp1E5kUglZG9QTqDEBLrcszO", "display-name": "Slack", "validation": "verified"}})
    }

    #[test]
    fn reads_the_store_and_says_what_it_would_refuse() {
        let snap = Snap::from_json(&slack());
        assert_eq!((snap.title.as_str(), snap.publisher.as_str()), ("Slack", "Slack"));
        assert_eq!(snap.size, Some(233066496));
        assert!(snap.verified() && snap.refusal().is_none());
        let mut classic = slack();
        classic["confinement"] = json!("classic");
        assert!(
            Snap::from_json(&classic)
                .refusal()
                .unwrap()
                .contains("strictly confined")
        );
        let mut unproven = slack();
        unproven["publisher"]["validation"] = json!("unproven");
        assert!(
            Snap::from_json(&unproven)
                .refusal()
                .unwrap()
                .contains("isn't verified")
        );
        let mut base = slack();
        base["type"] = json!("base");
        assert!(Snap::from_json(&base).refusal().unwrap().contains("isn't an app"));
    }

    #[test]
    fn names_follow_the_stores_rule() {
        for good in ["slack", "zoom-client", "obsidian", "a1"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "Slack",
            "-slack",
            "slack-",
            "slack--x",
            "123",
            "",
            "slack x",
            "slack/../x",
            &"a".repeat(41),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn parses_snapds_responses() {
        let raw = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"result\": [], \"status-code\": 200}";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["status-code"], 200);
        assert!(parse_response(b"garbage").is_err());
    }

    #[tokio::test]
    async fn asks_snapd_over_its_socket() {
        let socket = std::env::temp_dir().join(format!(
            "snapd-{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..10]
        ));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 1024];
                let n = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]).to_string();
                let body = if request.starts_with("GET /v2/find?name=slack ") {
                    json!({"result": [slack()]})
                } else {
                    json!({"result": {"message": "snap not installed"}})
                };
                let status = if request.contains("/v2/snaps/") {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let reply = format!("HTTP/1.0 {status}\r\nContent-Type: application/json\r\n\r\n{body}");
                stream.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let path = socket.to_str().unwrap();
        let snap = find(path, "slack").await.unwrap().unwrap();
        assert_eq!(snap.version, "4.53.292");
        assert_eq!(installed(path, "slack").await.unwrap(), None);
        assert!(find(path, "Not A Name").await.is_err());
        let _ = std::fs::remove_file(&socket);
    }
}
