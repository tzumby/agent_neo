//! A persistent `computer-use mcp` process: MCP over stdio, newline-delimited
//! JSON-RPC (docs/desktop-apps.md §2). Started once, initialized, and its
//! tools checked against the pinned release before the first call. Stdout
//! carries only protocol; each line is bounded, and each call has a deadline.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// The longest line HCU may send: a screenshot is a few hundred KB of base64.
pub const MAX_LINE: u64 = 16 * 1024 * 1024;
/// MCP versions HCU 1.0.1 speaks; the newer one first.
const PROTOCOL: &str = "2025-06-18";
/// What `initialize` may take: HCU starts its provider lazily, so this is quick.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// What it takes to start HCU on the desktop session.
#[derive(Clone, Debug)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The session's environment (DISPLAY, XAUTHORITY, HOME, XDG_RUNTIME_DIR,
    /// DBUS_SESSION_BUS_ADDRESS) plus HCU's own settings; nothing inherited.
    pub env: BTreeMap<String, String>,
    /// SHA-256 of each tool's input schema (canonical JSON) in the pinned
    /// release; empty skips the check (tests).
    pub schemas: BTreeMap<String, String>,
}

/// One tool call's result: HCU's text blocks (the evidence line first) and
/// images, kept apart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolResult {
    pub is_error: bool,
    pub text: String,
    pub images: Vec<(String, String)>,
}

/// The MCP session over any byte stream: a child's stdio, or a test's pipes.
pub struct Session<R, W> {
    reader: R,
    writer: W,
    next_id: u64,
}

impl<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin> Session<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 0,
        }
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn read_line(&mut self) -> Result<Value> {
        let mut line = Vec::new();
        let read = (&mut self.reader)
            .take(MAX_LINE)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            bail!("computer-use closed its output");
        }
        if line.last() != Some(&b'\n') {
            bail!("computer-use sent a line over {MAX_LINE} bytes");
        }
        serde_json::from_slice(&line).context("computer-use sent something other than JSON-RPC")
    }

    /// One request, and its response. Notifications in between are skipped.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let message = self.read_line().await?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!(
                    "computer-use refused {method}: {}",
                    error["message"].as_str().unwrap_or("no message")
                );
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("computer-use answered {method} without a result"));
        }
    }

    /// The handshake, then the tools: exactly AX and GUI, with the pinned
    /// release's schemas.
    pub async fn initialize(&mut self, schemas: &BTreeMap<String, String>) -> Result<String> {
        let init = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL,
                    "capabilities": {},
                    "clientInfo": {"name": "neo-desktop-broker", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        let version = init["serverInfo"]["version"].as_str().unwrap_or("").to_string();
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        let tools = self.request("tools/list", json!({})).await?;
        let tools = tools["tools"].as_array().cloned().unwrap_or_default();
        let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        names.sort_unstable();
        if names != ["AX", "GUI"] {
            bail!("computer-use offers {names:?}, not exactly AX and GUI");
        }
        for tool in &tools {
            let name = tool["name"].as_str().unwrap_or_default();
            let Some(expected) = schemas.get(name) else {
                continue;
            };
            let found = schema_digest(&tool["inputSchema"]);
            if &found != expected {
                bail!("computer-use's {name} schema isn't the pinned release's (sha256 {found})");
            }
        }
        Ok(version)
    }

    pub async fn call(&mut self, tool: &str, arguments: Value) -> Result<ToolResult> {
        let result = self
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await?;
        let mut out = ToolResult {
            is_error: result["isError"] == true,
            ..ToolResult::default()
        };
        let mut texts = Vec::new();
        for block in result["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => texts.push(block["text"].as_str().unwrap_or_default().to_string()),
                Some("image") => out.images.push((
                    block["mimeType"].as_str().unwrap_or("image/png").to_string(),
                    block["data"].as_str().unwrap_or_default().to_string(),
                )),
                _ => {}
            }
        }
        out.text = texts.join("\n");
        Ok(out)
    }
}

/// SHA-256 of a schema's canonical JSON: keys sorted (serde_json's map is
/// ordered), no spaces.
pub fn schema_digest(schema: &Value) -> String {
    let bytes = serde_json::to_vec(schema).unwrap_or_default();
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &bytes);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The child process and its session.
pub struct Hcu {
    child: Child,
    session: Session<BufReader<tokio::process::ChildStdout>, tokio::process::ChildStdin>,
    pub version: String,
}

impl Hcu {
    /// Starts `computer-use mcp` with only the session's environment and
    /// initializes it. Its stderr is kept (the last lines) for a failed start,
    /// never logged: it can quote the apps.
    pub async fn start(spawn: &Spawn, stderr_tail: Arc<Mutex<Vec<String>>>) -> Result<Self> {
        let mut child = Command::new(&spawn.program)
            .args(&spawn.args)
            .env_clear()
            .envs(&spawn.env)
            .current_dir(spawn.env.get("HOME").map_or("/", String::as_str))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", spawn.program.display()))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut tail = stderr_tail.lock().unwrap();
                    tail.push(line.chars().take(300).collect());
                    let excess = tail.len().saturating_sub(10);
                    tail.drain(..excess);
                }
            });
        }
        let mut session = Session::new(BufReader::new(stdout), stdin);
        let version = tokio::time::timeout(START_TIMEOUT, session.initialize(&spawn.schemas))
            .await
            .map_err(|_| {
                anyhow!(
                    "computer-use didn't initialize within {}s",
                    START_TIMEOUT.as_secs()
                )
            })??;
        Ok(Self {
            child,
            session,
            version,
        })
    }

    /// One call with a deadline. On timeout or a broken stream the process is
    /// no use any more (a response may still be on its way); the caller drops it.
    pub async fn call(&mut self, tool: &str, arguments: Value, deadline: Duration) -> Result<ToolResult> {
        tokio::time::timeout(deadline, self.session.call(tool, arguments))
            .await
            .map_err(|_| anyhow!("computer-use didn't answer within {}s", deadline.as_secs()))?
    }

    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[cfg(test)]
pub mod fake {
    //! A scripted `computer-use mcp` on in-memory pipes.
    use super::*;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    pub type Pipes = Session<BufReader<ReadHalf<DuplexStream>>, WriteHalf<DuplexStream>>;

    /// A session talking to `serve`, which gets each tools/call's name and
    /// arguments and returns the result object (content blocks).
    pub fn session(mut serve: impl FnMut(&str, &Value) -> Value + Send + 'static) -> Pipes {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (read, write) = tokio::io::split(ours);
        let (their_read, mut their_write) = tokio::io::split(theirs);
        tokio::spawn(async move {
            let mut lines = BufReader::new(their_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let message: Value = serde_json::from_str(&line).unwrap();
                let Some(id) = message.get("id").cloned() else {
                    continue;
                };
                let result = match message["method"].as_str().unwrap() {
                    "initialize" => {
                        json!({"protocolVersion": PROTOCOL, "serverInfo": {"name": "computer-use", "version": "1.0.1"}, "capabilities": {"tools": {}}})
                    }
                    "tools/list" => json!({"tools": [
                        {"name": "AX", "inputSchema": {"type": "object", "properties": {"mode": {"type": "string"}}}},
                        {"name": "GUI", "inputSchema": {"type": "object"}},
                    ]}),
                    "tools/call" => serve(
                        message["params"]["name"].as_str().unwrap(),
                        &message["params"]["arguments"],
                    ),
                    other => panic!("unexpected {other}"),
                };
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                if their_write.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        Session::new(BufReader::new(read), write)
    }

    pub fn text(text: &str, is_error: bool) -> Value {
        json!({"content": [{"type": "text", "text": text}], "isError": is_error})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initializes_checks_the_tools_and_calls() {
        let mut session = fake::session(|tool, args| {
            assert_eq!(tool, "AX");
            assert_eq!(args["mode"], "apps");
            json!({"content": [
                {"type": "text", "text": "{\"ok\":true,\"input_emitted\":false}"},
                {"type": "text", "text": "w1 \"Notes - Mousepad\"  app=org.xfce.mousepad  active"},
                {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="},
            ], "isError": false})
        });
        let pinned = BTreeMap::from([("GUI".to_string(), schema_digest(&json!({"type": "object"})))]);
        assert_eq!(session.initialize(&pinned).await.unwrap(), "1.0.1");
        let result = session.call("AX", json!({"mode": "apps"})).await.unwrap();
        assert!(!result.is_error);
        assert!(result.text.starts_with("{\"ok\":true"));
        assert!(result.text.ends_with("active"));
        assert_eq!(
            result.images,
            vec![("image/png".to_string(), "iVBORw0KGgo=".to_string())]
        );
    }

    #[tokio::test]
    async fn refuses_a_release_with_other_schemas() {
        let mut session = fake::session(|_, _| fake::text("", false));
        let pinned = BTreeMap::from([("AX".to_string(), "0".repeat(64))]);
        let err = session.initialize(&pinned).await.unwrap_err();
        assert!(format!("{err}").contains("isn't the pinned release's"), "{err}");
    }

    #[test]
    fn schema_digests_are_canonical() {
        let a: Value = serde_json::from_str(r#"{"b": 1, "a": [true, "x"]}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":[true,"x"],"b":1}"#).unwrap();
        assert_eq!(schema_digest(&a), schema_digest(&b));
        assert_eq!(schema_digest(&a).len(), 64);
    }
}
