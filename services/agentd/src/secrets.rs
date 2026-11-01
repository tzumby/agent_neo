//! Keys and tokens the user gives the agent: a GitHub token, an API key.
//!
//! They come from the app over the tailnet (end to end) and are kept here, one
//! file per key in agentd's state directory, readable by agentd and root only.
//! A command gets only the keys it names, as environment variables, through
//! systemd credentials: never on a command line or in a unit's properties.
//!
//! The model never sees a value. It knows the names; agentd replaces every
//! value with `[secret NAME]` in tool results, the event log (so the apps),
//! notifications and the chat history.

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
