//! Keys and tokens the user gives the agent: a GitHub token, an API key.
//!
//! They come from the app over the tailnet (end to end) and are kept here, one
//! file per key in agentd's state directory, readable by agentd and root only.
//! A command gets only the keys it names, as environment variables, through
//! systemd credentials: never on a command line or in a unit's properties.
//!
//! The model never sees a value. It knows the names; agentd replaces every
//! value with `[secret NAME]` in tool results, the event log (so the apps)
//! and the chat history.

use std::borrow::Cow;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::Value;

/// Shorter values would be redacted inside ordinary words.
pub const MIN_LEN: usize = 8;
pub const MAX_LEN: usize = 8192;
pub const MAX_SECRETS: usize = 50;

/// Variables a key may not replace: they change how every command runs.
const RESERVED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PWD",
    "OLDPWD",
    "TERM",
    "LANG",
    "TMPDIR",
    "IFS",
    "ENV",
    "PS1",
    "PS4",
    "PROMPT_COMMAND",
    "CREDENTIALS_DIRECTORY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];
const RESERVED_PREFIXES: &[&str] = &["LD_", "BASH_", "NIX_", "SYSTEMD_", "XDG_", "DBUS_"];

static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9_]{1,63}$").unwrap());
/// `$NAME` or `${NAME}` (also `${NAME:-…}`) in a command.
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{?([A-Z][A-Z0-9_]*)").unwrap());

/// Well-known key and token formats (the app's KeyDetector, ported): each has
/// a prefix, so ordinary text and hashes don't match.
static KEY_FORMATS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"\bgithub_pat_[A-Za-z0-9_]{40,}", "a GitHub token"),
        (r"\bgh[pousr]_[A-Za-z0-9]{30,}", "a GitHub token"),
        (r"\bglpat-[A-Za-z0-9_\-]{20,}", "a GitLab token"),
        (r"\bsk-ant-[A-Za-z0-9_\-]{20,}", "an Anthropic API key"),
        (r"\btgp_v1_[A-Za-z0-9_\-]{20,}", "a Together AI API key"),
        (r"\b[sr]k_(live|test)_[A-Za-z0-9]{20,}", "a Stripe API key"),
        (r"\bsk-[A-Za-z0-9_\-]{20,}", "an OpenAI API key"),
        (r"\bxox[abposr]-[A-Za-z0-9\-]{10,}", "a Slack token"),
        (r"\bhf_[A-Za-z0-9]{30,}", "a Hugging Face token"),
        (r"\bAIza[0-9A-Za-z_\-]{35}", "a Google API key"),
        (r"\bAKIA[0-9A-Z]{16}\b", "an AWS access key"),
    ]
    .into_iter()
    .map(|(pattern, what)| (Regex::new(pattern).unwrap(), what))
    .collect()
});
/// 13 to 19 digits, maybe in groups: a card number if it passes Luhn.
static DIGIT_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d(?:[ -]?\d){12,18}\b").unwrap());
/// A password word, then up to 40 characters to look at.
static PASSWORD_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i:\b(?:password|passcode|passwd)\b)|\bPIN\b").unwrap());
/// A 6–8 digit code near "code", "OTP" or "2FA" (either side, within 20 characters).
static ONE_TIME_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:code|otp|2fa)\b.{0,20}?\b\d{6,8}\b|\b\d{6,8}\b.{0,20}?\b(?:code|otp|2fa)\b")
        .unwrap()
});

fn luhn(digits: &[u32]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                if d * 2 > 9 { d * 2 - 9 } else { d * 2 }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// A token right after a password word that isn't a plain word: digits or
/// symbols in it ("hunter2", "Tr0ub4dor&3"). "The PIN pad is by the door"
/// passes.
fn secret_after_password_word(rest: &str) -> bool {
    let window: String = rest.chars().take(40).collect();
    window
        .split(|c: char| c.is_whitespace() || matches!(c, ':' | '=' | ',' | ';' | '"' | '\'' | '(' | ')'))
        .filter(|token| token.chars().count() >= 3)
        .any(|token| {
            let token = token.trim_end_matches(['.', '!', '?']);
            token.chars().any(|c| c.is_ascii_digit())
                || token
                    .chars()
                    .any(|c| !(c.is_alphabetic() || c == '-' || c == '\''))
        })
}

/// What a memory's text holds that must never be stored, if anything: a key
/// or token, a card number, a password, a one-time code. Emails, phone
/// numbers, names and addresses pass: drafting mail needs them.
pub fn looks_like_secret(text: &str) -> Option<&'static str> {
    if let Some((_, what)) = KEY_FORMATS.iter().find(|(re, _)| re.is_match(text)) {
        return Some(what);
    }
    for run in DIGIT_RUN.find_iter(text) {
        let digits: Vec<u32> = run.as_str().chars().filter_map(|c| c.to_digit(10)).collect();
        if (13..=19).contains(&digits.len()) && luhn(&digits) {
            return Some("a card number");
        }
    }
    if PASSWORD_WORD
        .find_iter(text)
        .any(|word| secret_after_password_word(&text[word.end()..]))
    {
        return Some("a password or PIN");
    }
    if ONE_TIME_CODE.is_match(text) {
        return Some("a one-time code");
    }
    None
}

/// Why a name can't be used, if it can't.
pub fn check_name(name: &str) -> Result<(), String> {
    if !NAME.is_match(name) {
        return Err(
            "a key's name is 2 to 64 capital letters, digits and underscores, starting with a letter, \
             like GITHUB_TOKEN"
                .into(),
        );
    }
    if ["PASSWORD", "PASSWD", "PASSCODE"]
        .iter()
        .any(|p| name.contains(p))
    {
        return Err(
            "only API keys and tokens can be saved, never passwords: the user signs in to sites \
             themselves, in the desktop's browser"
                .into(),
        );
    }
    if RESERVED.contains(&name) || RESERVED_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Err(format!("{name} is used by the system; choose another name"));
    }
    Ok(())
}

/// The value as it's kept (trimmed), or why it can't be.
pub fn check_value(value: &str) -> Result<&str, String> {
    let value = value.trim();
    let length = value.chars().count();
    if length < MIN_LEN {
        return Err(format!(
            "that's too short for a key (at least {MIN_LEN} characters)"
        ));
    }
    if value.len() > MAX_LEN {
        return Err(format!("that's too long for a key (at most {MAX_LEN} bytes)"));
    }
    if value.chars().any(char::is_control) {
        return Err("a key is one line, without spaces or control characters".into());
    }
    Ok(value)
}

pub struct Secrets {
    dir: PathBuf,
    /// (name, value), longest value first, so a value inside another is
    /// redacted after it.
    values: RwLock<Vec<(String, String)>>,
}

impl Secrets {
    /// Keeps keys in `dir` (created, mode 0700), loading those already there.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let mut values = Vec::new();
        for entry in std::fs::read_dir(dir)?.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if check_name(&name).is_err() {
                continue; // temporary files and anything else
            }
            match std::fs::read_to_string(entry.path()) {
                Ok(value) => values.push((name, value.trim().to_string())),
                Err(err) => tracing::warn!(%err, name, "couldn't read a saved key"),
            }
        }
        let secrets = Self {
            dir: dir.to_path_buf(),
            values: RwLock::new(Vec::new()),
        };
        secrets.set(values);
        Ok(secrets)
    }

    fn set(&self, mut values: Vec<(String, String)>) {
        values.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        *self.values.write().unwrap() = values;
    }

    /// The file systemd loads the key from (LoadCredential).
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn has(&self, name: &str) -> bool {
        self.values.read().unwrap().iter().any(|(n, _)| n == name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .values
            .read()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        names.sort();
        names
    }

    /// Saves a key (replacing one of that name): written to a temporary file
    /// with mode 0600, then renamed into place.
    pub fn put(&self, name: &str, value: &str) -> Result<()> {
        check_name(name).map_err(anyhow::Error::msg)?;
        let value = check_value(value).map_err(anyhow::Error::msg)?;
        let temporary = self.dir.join(format!(".{name}.new"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(value.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, self.path(name))?;
        let mut values: Vec<(String, String)> = self
            .values
            .read()
            .unwrap()
            .iter()
            .filter(|(n, _)| n != name)
            .cloned()
            .collect();
        values.push((name.to_string(), value.to_string()));
        self.set(values);
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<bool> {
        if check_name(name).is_err() || !self.has(name) {
            return Ok(false);
        }
        match std::fs::remove_file(self.path(name)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        let values = self
            .values
            .read()
            .unwrap()
            .iter()
            .filter(|(n, _)| n != name)
            .cloned()
            .collect();
        self.set(values);
        Ok(true)
    }

    /// The saved keys a command uses: those it names (`$NAME`, `${NAME}`) and
    /// those asked for. Also returns the asked-for names that aren't saved.
    pub fn used_by(&self, command: &str, asked: &[String]) -> (Vec<String>, Vec<String>) {
        let mut used = Vec::new();
        let mut unknown = Vec::new();
        let named = REFERENCE.captures_iter(command).map(|c| c[1].to_string());
        for name in asked.iter().map(|n| n.trim().to_string()).chain(named) {
            if used.contains(&name) || unknown.contains(&name) {
                continue;
            }
            if self.has(&name) {
                used.push(name);
            } else if asked.iter().any(|a| a.trim() == name) {
                unknown.push(name);
            }
        }
        used.sort();
        (used, unknown)
    }

    /// The text with every saved value replaced by `[secret NAME]`.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let values = self.values.read().unwrap();
        let mut text = Cow::Borrowed(text);
        for (name, value) in values.iter() {
            if value.len() >= MIN_LEN && text.contains(value.as_str()) {
                text = Cow::Owned(text.replace(value.as_str(), &format!("[secret {name}]")));
            }
        }
        text
    }

    /// Redacts every string in a JSON value.
    /// Whether the text holds a saved key's value.
    pub fn holds_a_value(&self, text: &str) -> bool {
        matches!(self.redact(text), Cow::Owned(_))
    }

    pub fn redact_json(&self, value: &mut Value) {
        if self.values.read().unwrap().is_empty() {
            return;
        }
        match value {
            Value::String(text) => {
                if let Cow::Owned(redacted) = self.redact(text) {
                    *text = redacted;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact_json(item)),
            Value::Object(fields) => fields.values_mut().for_each(|field| self.redact_json(field)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temporary() -> PathBuf {
        std::env::temp_dir().join(format!("agentd-secrets-{}", uuid::Uuid::new_v4().simple()))
    }

    #[test]
    fn names_are_env_vars_never_passwords_or_system_ones() {
        assert!(check_name("GITHUB_TOKEN").is_ok());
        assert!(check_name("TOGETHER_API_KEY").is_ok());
        for bad in [
            "github_token",
            "1TOKEN",
            "A",
            "MY-KEY",
            "GMAIL_PASSWORD",
            "PATH",
            "LD_PRELOAD",
            "BASH_ENV",
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert_eq!(check_value("  ghp_abcdefghijkl \n"), Ok("ghp_abcdefghijkl"));
        assert!(check_value("short").is_err());
        assert!(check_value("two\nlines-of-key").is_err());
    }

    #[test]
    fn spots_secrets_but_lets_contact_details_through() {
        for text in [
            "my token is ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab",
            "github_pat_11ABCDEFG0abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ",
            "OpenAI key sk-proj-abcdefghijklmnopqrstuvwxyz012345",
            "Anthropic sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            "together tgp_v1_abcdefghijklmnopqrstuvwxyz",
            "slack xoxb-1234567890-abcdefghij",
            "AWS AKIAIOSFODNN7EXAMPLE",
            "AIzaSyA-abcdefghijklmnopqrstuvwxyz012345",
            "hf_abcdefghijklmnopqrstuvwxyzABCDEF",
            "stripe sk_live_abcdefghijklmnopqrstu",
            "gitlab glpat-abcdefghijklmnopqrst",
            "card 4242 4242 4242 4242 expires 12/29",
            "the Visa is 4111111111111111",
            "password: hunter2",
            "Her wifi passcode is Tr0ub4dor&3",
            "PIN 4821 for the garage",
            "the login code is 384912",
            "OTP 55120933",
            "812345 is the 2FA code",
        ] {
            assert!(looks_like_secret(text).is_some(), "{text}");
        }
        for text in [
            "Email invoices to billing@example.com",
            "Call Dana at +1 416 555 0199",
            "The office is at 100 King St W, Toronto, M5X 1A9",
            "The PIN pad is by the door",
            "Remember to reset your password every quarter",
            "Order number 1234567890123 shipped",
            "Use the code review checklist",
            "The meeting room code of conduct",
        ] {
            assert_eq!(looks_like_secret(text), None, "{text}");
        }
    }

    #[test]
    fn saved_keys_are_private_files_and_survive_a_restart() {
        let dir = temporary();
        let secrets = Secrets::open(&dir).unwrap();
        secrets
            .put("GITHUB_TOKEN", " github_pat_11ABCDEFG0123456789\n")
            .unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&secrets.path("GITHUB_TOKEN")), 0o600);
        assert_eq!(
            std::fs::read_to_string(secrets.path("GITHUB_TOKEN")).unwrap(),
            "github_pat_11ABCDEFG0123456789"
        );
        assert!(secrets.put("lower", "abcdefghijk").is_err());

        let reopened = Secrets::open(&dir).unwrap();
        assert_eq!(reopened.names(), ["GITHUB_TOKEN"]);
        assert!(reopened.remove("GITHUB_TOKEN").unwrap());
        assert!(!reopened.remove("GITHUB_TOKEN").unwrap());
        assert!(!reopened.path("GITHUB_TOKEN").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn values_are_redacted_everywhere() {
        let dir = temporary();
        let secrets = Secrets::open(&dir).unwrap();
        secrets.put("TOGETHER_API_KEY", "tgp_v1_secretvalue").unwrap();
        secrets.put("SHORTER", "secretvalue").unwrap();
        assert_eq!(
            secrets.redact("key=tgp_v1_secretvalue; other=secretvalue"),
            "key=[secret TOGETHER_API_KEY]; other=[secret SHORTER]"
        );
        assert!(matches!(secrets.redact("nothing here"), Cow::Borrowed(_)));
        let mut event =
            json!({"output": ["Bearer tgp_v1_secretvalue"], "n": 1, "nested": {"x": "secretvalue!"}});
        secrets.redact_json(&mut event);
        assert_eq!(
            event,
            json!({"output": ["Bearer [secret TOGETHER_API_KEY]"], "n": 1, "nested": {"x": "[secret SHORTER]!"}})
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_command_gets_the_keys_it_names_or_asks_for() {
        let dir = temporary();
        let secrets = Secrets::open(&dir).unwrap();
        secrets.put("GITHUB_TOKEN", "github_pat_0123456789").unwrap();
        secrets.put("TOGETHER_API_KEY", "tgp_v1_0123456789").unwrap();
        let (used, unknown) = secrets.used_by("echo \"${GITHUB_TOKEN}\" $HOME", &[]);
        assert_eq!((used, unknown), (vec!["GITHUB_TOKEN".to_string()], vec![]));
        let (used, unknown) = secrets.used_by(
            "opencode run hi",
            &["TOGETHER_API_KEY".into(), "OPENAI_API_KEY".into()],
        );
        assert_eq!(used, ["TOGETHER_API_KEY"]);
        assert_eq!(unknown, ["OPENAI_API_KEY"]);
        assert_eq!(secrets.used_by("ls -la", &[]), (vec![], vec![]));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
