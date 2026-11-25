//! agentd's side of the broker's socket: one connection per request.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::{Request, Response, refusal};

/// The broker answers a request within its own deadlines (a minute for the
/// one before, a minute for a click); this is the outer bound.
const DEADLINE: Duration = Duration::from_secs(150);
const MAX_RESPONSE: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Desktop {
    socket: PathBuf,
}

impl Desktop {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The broker's answer, or an `unavailable` refusal saying why there's none.
    /// Dropping the future closes the connection: the broker then treats a
    /// click it was running as unseen (broker.rs).
    pub async fn request(&self, request: &Request) -> Response {
        match tokio::time::timeout(DEADLINE, self.exchange(request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => Response::refused(
                refusal::UNAVAILABLE,
                format!("the desktop's app broker isn't answering ({err})"),
            ),
            Err(_) => Response::refused(
                refusal::UNAVAILABLE,
                "the desktop's app broker didn't answer in time",
            ),
        }
    }

    async fn exchange(&self, request: &Request) -> std::io::Result<Response> {
        let mut stream = tokio::net::UnixStream::connect(&self.socket).await?;
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        stream.write_all(&line).await?;
        let mut reply = Vec::new();
        BufReader::new(stream)
            .take(MAX_RESPONSE)
            .read_until(b'\n', &mut reply)
            .await?;
        if reply.is_empty() {
            return Err(std::io::Error::other("it closed the connection"));
        }
        Ok(serde_json::from_slice(&reply)?)
    }
}
