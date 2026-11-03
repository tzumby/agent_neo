//! Where the agent posts updates: Slack and Microsoft Teams channels a person
//! set up in the web app (docs/architecture.md §6.20). The model picks one by
//! name with post_update; it can't add one, or post anywhere else.
//!
//! What it writes often comes from logs, which attackers write too (request
//! paths, user agents, form fields). So whatever the model asks, every post
//! here has links defanged (hxxps://evil[.]com, never clickable),
//! @channel/@here/@everyone neutralized, Slack's markup escaped, well-known
//! key formats and card numbers cut out, link previews off, and a rate limit
//! per destination. The only link is agentd's own, back to the web app.
//!
//! Slack: a bot token (xoxb-, scopes chat:write and chat:write.public) from an
//! app the customer installs; chat.postMessage returns the message's ts, so
//! follow-ups go in its thread. Teams: a Workflows webhook ("Send webhook
//! alerts to a channel"); no threads there. Both outbound HTTPS only.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};

/// Posts per destination per rolling hour.
pub const POSTS_PER_HOUR: i64 = 20;
/// The same update to the same place within this many seconds is refused.
pub const DEDUPE_SECS: i64 = 6 * 3600;
pub const MAX_DESTINATIONS: usize = 10;
pub const MAX_TITLE: usize = 150;
pub const MAX_BODY: usize = 6000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Slack,
    Teams,
}

impl Kind {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "slack" => Some(Self::Slack),
            "teams" => Some(Self::Teams),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Teams => "teams",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn parse(text: Option<&str>) -> Self {
        match text {
            Some("critical") => Self::Critical,
            Some("warning") => Self::Warning,
            _ => Self::Info,
        }
    }
}

/// A destination as the web app lists it: never its token or URL.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Destination {
    pub name: String,
    pub kind: Kind,
    /// Slack: "#security" or a channel ID. Teams: empty (the URL is the channel).
    pub channel: String,
    /// The web app's address, for the link back (set by the web app itself).
    pub link: String,
    pub created: i64,
    pub by: Option<String>,
    pub last_post: Option<i64>,
    pub last_error: Option<String>,
}

impl Destination {
    /// "Slack #security", "Teams".
    pub fn describe(&self) -> String {
        match self.kind {
            Kind::Slack => format!("Slack {}", self.channel),
            Kind::Teams => "a Microsoft Teams channel".to_string(),
        }
    }
}

static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9-]{1,30}$").unwrap());
static SLACK_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[CG][A-Z0-9]{8,12}$").unwrap());
static SLACK_CHANNEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#[a-z0-9][a-z0-9._-]{0,79}|[CG][A-Z0-9]{8,12})$").unwrap());

pub fn check_name(name: &str) -> Result<(), String> {
    if NAME.is_match(name) {
        Ok(())
    } else {
        Err("a destination's name is 2 to 31 lowercase letters, digits and dashes, like security".into())
    }
}

/// Where its token or URL is kept: in a store of its own, which no command
/// is ever given (unlike saved keys).
pub fn credential_name(name: &str) -> String {
    format!("DEST_{}", name.to_uppercase().replace('-', "_"))
}

/// "#Security" → "#security"; a channel ID stays as it is.
pub fn check_slack(channel: &str, token: Option<&str>) -> Result<String, String> {
    let channel = channel.trim();
    let channel = if SLACK_ID.is_match(channel) {
        channel.to_string()
    } else {
        format!("#{}", channel.trim_start_matches('#').to_lowercase())
    };
    if !SLACK_CHANNEL.is_match(&channel) {
        return Err("a Slack channel is its name (#security) or its ID (C0123ABCD)".into());
    }
    if let Some(token) = token
        && !token.trim().starts_with("xoxb-")
    {
        return Err(
            "that isn't a Slack bot token: it starts with xoxb- (OAuth & Permissions in the app's settings)"
                .into(),
        );
    }
    Ok(channel)
}

/// A Workflows webhook: https on Microsoft's Logic Apps or Power Platform hosts.
pub fn check_teams(url: &str) -> Result<(), String> {
    let wrong = || {
        "that isn't a Teams Workflows webhook: copy the URL the \"Send webhook alerts to a channel\" \
         workflow gives you (https://…logic.azure.com/… or https://…api.powerplatform.com/…)"
            .to_string()
    };
    let parsed = url::Url::parse(url.trim()).map_err(|_| wrong())?;
    let host = parsed.host_str().unwrap_or_default();
    let microsoft = host.ends_with(".logic.azure.com") || host.ends_with(".api.powerplatform.com");
    if parsed.scheme() != "https" || !microsoft || parsed.port().is_some_and(|p| p != 443) {
        return Err(wrong());
    }
    Ok(())
}

/// The web app's own address, for the link back: https, or http on this
/// machine for local development. Just the origin.
pub fn check_link(link: &str) -> Result<String, String> {
    if link.is_empty() {
        return Ok(String::new());
    }
    let parsed = url::Url::parse(link).map_err(|_| "the web app's address isn't a URL".to_string())?;
    let local = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && local) {
        return Err("the web app's address must be https".into());
    }
    Ok(parsed.origin().ascii_serialization())
}

// --- What a post says ----------------------------------------------------------

static URL_IN_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\b(http|ftp)(s?)://([^\s<>()\[\]"'`|]+)"#).unwrap());
static MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!?\[([^\]\n]*)\]\(([^)\s]+)[^)]*\)").unwrap());
static BROADCAST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)@(channel|here|everyone|all)\b").unwrap());
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s{0,3}#{1,6}\s+(.*?)\s*#*\s*$").unwrap());
static BOLD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*\*(\S(?:.*?\S)?)\*\*|__(\S(?:.*?\S)?)__").unwrap());
static ITALIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*(\S(?:[^*]*?\S)?)\*").unwrap());
static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\s*)[-*+]\s+").unwrap());

/// hxxps://evil[.]com/x: still readable and copyable, never a link.
pub fn defang(text: &str) -> String {
    URL_IN_TEXT
        .replace_all(text, |c: &regex::Captures| {
            let scheme = if c[1].eq_ignore_ascii_case("http") {
                "hxxp"
            } else {
                "fxp"
            };
            let rest = &c[3];
            let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            format!("{scheme}{}://{}{path}", &c[2], host.replace('.', "[.]"))
        })
        .into_owned()
}

/// Everything both formats do first: Markdown links become "text (url)",
/// URLs are defanged, broadcasts can't ping, keys are cut out.
pub fn neutralize(text: &str) -> String {
    let (text, _) = crate::secrets::scrub_keys(text);
    let text = MD_LINK.replace_all(&text, |c: &regex::Captures| {
        if c[1].trim().is_empty() || c[1] == c[2] {
            c[2].to_string()
        } else {
            format!("{} ({})", &c[1], &c[2])
        }
    });
    let text = defang(&text);
    // A word joiner after the @: it reads the same and pings no one.
    BROADCAST.replace_all(&text, "@\u{2060}$1").into_owned()
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

/// Slack's markup characters, escaped: no <!channel>, <@U…> or <url|text>
/// can come from the text.
fn slack_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The agent's Markdown as Slack mrkdwn: **bold** → *bold*, *italic* →
/// _italic_, headings bold, bullets as •, tables and code in code blocks.
pub fn slack_mrkdwn(markdown: &str) -> String {
    let mut out = Vec::new();
    let mut in_code = false;
    let mut table: Vec<String> = Vec::new();
    let flush_table = |table: &mut Vec<String>, out: &mut Vec<String>| {
        if !table.is_empty() {
            out.push(format!("```\n{}\n```", table.join("\n")));
            table.clear();
        }
    };
    for line in neutralize(markdown).lines() {
        let escaped = slack_escape(line);
        if line.trim_start().starts_with("```") {
            flush_table(&mut table, &mut out);
            in_code = !in_code;
            out.push("```".into());
            continue;
        }
        if in_code {
            out.push(escaped);
            continue;
        }
        if line.trim_start().starts_with('|') {
            table.push(escaped);
            continue;
        }
        flush_table(&mut table, &mut out);
        if let Some(c) = HEADING.captures(&escaped) {
            out.push(format!("*{}*", BOLD.replace_all(&c[1], "$1$2")));
            continue;
        }
        let line = BULLET.replace(&escaped, "$1• ");
        // Bold through a placeholder, so the italic rule doesn't take it.
        let line = BOLD.replace_all(&line, "\u{1}$1$2\u{1}");
        let line = ITALIC.replace_all(&line, "_${1}_");
        out.push(line.replace('\u{1}', "*"));
    }
    flush_table(&mut table, &mut out);
    if in_code {
        out.push("```".into());
    }
    out.join("\n")
}

/// Slack sections hold 3,000 characters: the text in pieces, split at lines.
fn sections(text: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    for line in text.lines() {
        let current = parts.last_mut().unwrap();
        if current.chars().count() + line.chars().count() + 1 > 2900 && !current.is_empty() {
            parts.push(String::new());
        }
        let current = parts.last_mut().unwrap();
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(&clip(line, 2900));
    }
    parts.retain(|p| !p.trim().is_empty());
    parts
}

pub struct Update<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub severity: Severity,
    /// Teams has no threads: the earlier update's title, said in this one.
    pub follows: Option<&'a str>,
}

fn severity_words(severity: Severity) -> Option<&'static str> {
    match severity {
        Severity::Critical => Some("🔴 Critical"),
        Severity::Warning => Some("🟠 Warning"),
        Severity::Info => None,
    }
}

/// chat.postMessage's body.
pub fn slack_payload(
    channel: &str,
    update: &Update,
    desktop: &str,
    link: &str,
    thread_ts: Option<&str>,
) -> Value {
    let title = clip(&neutralize(update.title), MAX_TITLE);
    let mut blocks =
        vec![json!({"type": "header", "text": {"type": "plain_text", "text": title, "emoji": true}})];
    if let Some(words) = severity_words(update.severity) {
        blocks.push(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": words}]}));
    }
    for part in sections(&slack_mrkdwn(&clip(update.body, MAX_BODY)))
        .into_iter()
        .take(40)
    {
        blocks.push(json!({"type": "section", "text": {"type": "mrkdwn", "text": part}}));
    }
    let footer = if link.is_empty() {
        format!("Posted by Neo Agent on {}.", slack_escape(desktop))
    } else {
        format!(
            "Posted by Neo Agent on {}. <{link}/#/chat|Open the chat>",
            slack_escape(desktop)
        )
    };
    blocks.push(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": footer}]}));
    let fallback = match severity_words(update.severity) {
        Some(words) => format!("{words}: {title}"),
        None => title.clone(),
    };
    let mut payload = json!({
        "channel": channel,
        "text": fallback,
        "blocks": blocks,
        "unfurl_links": false,
        "unfurl_media": false,
        "link_names": false,
    });
    if let Some(ts) = thread_ts {
        payload["thread_ts"] = json!(ts);
    }
    payload
}

/// The Workflows webhook's body: one Adaptive Card. Code and tables in
/// monospace; the rest as the card's own small Markdown.
pub fn teams_payload(update: &Update, desktop: &str, link: &str) -> Value {
    let title = clip(&neutralize(update.title), MAX_TITLE);
    let color = match update.severity {
        Severity::Critical => "Attention",
        Severity::Warning => "Warning",
        Severity::Info => "Default",
    };
    let mut body = vec![
        json!({"type": "TextBlock", "text": title, "size": "Medium", "weight": "Bolder", "wrap": true, "color": color}),
    ];
    if let Some(words) = severity_words(update.severity) {
        body.push(
            json!({"type": "TextBlock", "text": words, "spacing": "None", "isSubtle": true, "wrap": true}),
        );
    }
    if let Some(earlier) = update.follows {
        body.push(json!({"type": "TextBlock", "text": format!("Follows: {}", clip(&neutralize(earlier), MAX_TITLE)), "isSubtle": true, "wrap": true}));
    }
    let mut prose: Vec<String> = Vec::new();
    let mut mono: Vec<String> = Vec::new();
    let mut in_code = false;
    let text = neutralize(&clip(update.body, MAX_BODY));
    let push_prose = |prose: &mut Vec<String>, body: &mut Vec<Value>| {
        let text = prose.join("\n").trim().to_string();
        if !text.is_empty() {
            body.push(json!({"type": "TextBlock", "text": text, "wrap": true}));
        }
        prose.clear();
    };
    let push_mono = |mono: &mut Vec<String>, body: &mut Vec<Value>| {
        if !mono.is_empty() {
            body.push(
                json!({"type": "TextBlock", "text": mono.join("\n"), "fontType": "Monospace", "wrap": true}),
            );
        }
        mono.clear();
    };
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            push_prose(&mut prose, &mut body);
            if in_code {
                push_mono(&mut mono, &mut body);
            }
            in_code = !in_code;
        } else if in_code || line.trim_start().starts_with('|') {
            if !in_code && mono.is_empty() {
                push_prose(&mut prose, &mut body);
            }
            mono.push(line.to_string());
        } else {
            push_mono(&mut mono, &mut body);
            let line = HEADING.replace(line, "**$1**");
            prose.push(line.into_owned());
        }
    }
    push_prose(&mut prose, &mut body);
    push_mono(&mut mono, &mut body);
    body.push(json!({"type": "TextBlock", "text": format!("Posted by Neo Agent on {desktop}."), "isSubtle": true, "size": "Small", "wrap": true}));
    let mut card = json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "body": body,
        "msteams": {"width": "Full"},
    });
    if !link.is_empty() {
        card["actions"] =
            json!([{"type": "Action.OpenUrl", "title": "Open the chat", "url": format!("{link}/#/chat")}]);
    }
    json!({
        "type": "message",
        "attachments": [{"contentType": "application/vnd.microsoft.card.adaptive", "contentUrl": null, "content": card}],
    })
}

// --- Sending -------------------------------------------------------------------

/// Sends posts: no redirects (a webhook can't send us elsewhere), a short
/// timeout, https only outside tests. Errors never carry the URL (a Teams
/// webhook's URL is its secret).
pub struct Poster {
    client: reqwest::Client,
    slack_api: String,
}

impl Poster {
    pub fn with(slack_api: &str, https_only: bool) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .https_only(https_only)
                .build()?,
            slack_api: slack_api.trim_end_matches('/').to_string(),
        })
    }

    /// chat.postMessage; the message's ts, or Slack's reason in plain words.
    pub async fn slack(&self, token: &str, payload: &Value) -> Result<String, String> {
        let reply = self
            .client
            .post(format!("{}/chat.postMessage", self.slack_api))
            .bearer_auth(token)
            .json(payload)
            .send()
            .await
            .map_err(|e| format!("couldn't reach Slack: {}", e.without_url()))?;
        let status = reply.status();
        let body: Value = reply.json().await.unwrap_or(Value::Null);
        if body["ok"] == true {
            return Ok(body["ts"].as_str().unwrap_or_default().to_string());
        }
        let channel = payload["channel"].as_str().unwrap_or("the channel");
        Err(match body["error"].as_str().unwrap_or_default() {
            "not_in_channel" => {
                format!("the Slack app isn't in {channel}: invite it there (/invite @the app)")
            }
            "channel_not_found" => format!(
                "Slack has no channel {channel} the app can see (a private channel needs the app invited)"
            ),
            "invalid_auth" | "not_authed" | "token_revoked" | "account_inactive" => {
                "Slack refused the bot token (revoked or wrong): replace it in Updates".to_string()
            }
            "missing_scope" => "the Slack app lacks chat:write: reinstall it from the manifest".to_string(),
            "ratelimited" => "Slack is rate-limiting posts: try again in a minute".to_string(),
            "" => format!("Slack answered {status}"),
            other => format!("Slack said {other}"),
        })
    }

    /// The Workflows webhook answers 202 Accepted.
    pub async fn teams(&self, url: &str, payload: &Value) -> Result<(), String> {
        let reply = self
            .client
            .post(url)
            .json(payload)
            .send()
            .await
            .map_err(|e| format!("couldn't reach Teams: {}", e.without_url()))?;
        let status = reply.status();
        if status.is_success() {
            Ok(())
        } else if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::UNAUTHORIZED {
            Err("Teams refused the webhook (deleted or turned off): set up the workflow again and replace its URL".into())
        } else {
            Err(format!("Teams answered {status}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_channels_and_addresses() {
        assert!(
            check_name("security").is_ok() && check_name("Security").is_err() && check_name("a").is_err()
        );
        assert_eq!(credential_name("on-call"), "DEST_ON_CALL");
        assert_eq!(check_slack("Security", Some("xoxb-1-2-x")).unwrap(), "#security");
        assert_eq!(check_slack("C0123ABCD", None).unwrap(), "C0123ABCD");
        assert!(check_slack("#security", Some("xoxp-user-token")).is_err());
        assert!(check_slack("#has space", None).is_err());
        assert!(
            check_teams(
                "https://prod-27.westus.logic.azure.com:443/workflows/abc/triggers/manual/paths/invoke?sig=x"
            )
            .is_ok()
        );
        assert!(check_teams("https://default123.ab.environment.api.powerplatform.com:443/powerautomate/automations/direct/workflows/x").is_ok());
        for wrong in [
            "http://prod-27.westus.logic.azure.com/x",
            "https://evil.com/logic.azure.com",
            "https://logic.azure.com.evil.com/x",
            "https://x.logic.azure.com:8443/x",
        ] {
            assert!(check_teams(wrong).is_err(), "{wrong}");
        }
        assert_eq!(
            check_link("https://agent.acme.com/#/chat").unwrap(),
            "https://agent.acme.com"
        );
        assert_eq!(
            check_link("http://127.0.0.1:8088/").unwrap(),
            "http://127.0.0.1:8088"
        );
        assert!(check_link("http://agent.acme.com").is_err() && check_link("javascript:alert(1)").is_err());
    }

    #[test]
    fn attacker_text_can_t_link_ping_or_leak() {
        let text = "Blocked https://evil.example.com/login?x=1 and [click](http://phish.example/a) <!channel> @here \
                    token ghp_abcdefghijklmnopqrstuvwxyz0123456789 card 4111 1111 1111 1111";
        let out = neutralize(text);
        assert!(out.contains("hxxps://evil[.]example[.]com/login?x=1"), "{out}");
        assert!(out.contains("click (hxxp://phish[.]example/a)"), "{out}");
        assert!(!out.contains("https://") && !out.contains("http://"));
        assert!(!out.contains("@here") && out.contains("@\u{2060}here"));
        assert!(!out.contains("ghp_") && out.contains("[removed: a GitHub token]"));
        assert!(!out.contains("4111 1111") && out.contains("[removed: a card number]"));
        let slack = slack_mrkdwn(text);
        assert!(
            !slack.contains("<!channel>") && slack.contains("&lt;!channel&gt;"),
            "{slack}"
        );
    }

    #[test]
    fn markdown_becomes_slack_mrkdwn() {
        let md = "## Login abuse\nThere were **312** bursts, *mostly* one ASN:\n- 41 addresses\n- `/api/login`\n\n| Rule | Hits |\n|--|--|\n| SQLi | 4 |\n```\nGET /api/login 429\n```";
        let out = slack_mrkdwn(md);
        assert!(out.starts_with("*Login abuse*"), "{out}");
        assert!(
            out.contains("There were *312* bursts, _mostly_ one ASN:"),
            "{out}"
        );
        assert!(out.contains("• 41 addresses") && out.contains("• `/api/login`"));
        assert!(
            out.contains("```\n| Rule | Hits |\n|--|--|\n| SQLi | 4 |\n```"),
            "{out}"
        );
        assert!(out.contains("```\nGET /api/login 429\n```"));
    }

    #[test]
    fn payloads() {
        let update = Update {
            title: "Credential stuffing on /api/login",
            body: "**312** bursts.",
            severity: Severity::Critical,
            follows: None,
        };
        let slack = slack_payload(
            "#security",
            &update,
            "acme-agent",
            "https://agent.acme.com",
            Some("1700000000.000100"),
        );
        assert_eq!(slack["channel"], "#security");
        assert_eq!(slack["thread_ts"], "1700000000.000100");
        assert_eq!(slack["unfurl_links"], false);
        assert_eq!(slack["text"], "🔴 Critical: Credential stuffing on /api/login");
        let blocks = slack["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "header");
        assert!(
            blocks.last().unwrap()["elements"][0]["text"]
                .as_str()
                .unwrap()
                .contains("<https://agent.acme.com/#/chat|Open the chat>")
        );

        let long = "line\n".repeat(2000);
        let big = Update {
            title: "t",
            body: &long,
            severity: Severity::Info,
            follows: None,
        };
        let blocks = slack_payload("#x", &big, "d", "", None)["blocks"]
            .as_array()
            .unwrap()
            .clone();
        assert!(
            blocks.len() <= 50
                && blocks.iter().all(|b| b["text"]["text"]
                    .as_str()
                    .is_none_or(|t| t.chars().count() <= 3000))
        );

        let teams = teams_payload(
            &Update {
                title: "Spike",
                body: "Text\n```\nGET /x\n```\nMore",
                severity: Severity::Warning,
                follows: Some("Earlier"),
            },
            "acme-agent",
            "https://agent.acme.com",
        );
        let card = &teams["attachments"][0]["content"];
        assert_eq!(card["type"], "AdaptiveCard");
        let texts: Vec<&str> = card["body"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts[0], "Spike");
        assert!(
            texts.contains(&"Follows: Earlier") && texts.contains(&"GET /x") && texts.contains(&"More"),
            "{texts:?}"
        );
        assert_eq!(card["actions"][0]["url"], "https://agent.acme.com/#/chat");
    }
}
