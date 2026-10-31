//! agentd: the agent on a cloud desktop. Runs the model loop with browser and
//! shell tools, keeps the chat history, and serves the chat API to the apps.

mod agent;
mod api;
mod aws;
mod browser;
mod cdp;
mod clock;
mod config;
mod guard;
mod llm;
mod policy;
mod push;
mod shell;
mod store;
mod tools;

use std::io::IsTerminal;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).is_some_and(|arg| arg == "--version") {
        println!("agentd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    // agentd runs as its own user; as root it would leave root-owned state.
    // /proc/self belongs to the process's effective user.
    if std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0) {
        anyhow::bail!("agentd must not run as root (it runs as the agentd user, under systemd)");
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("agentd=info")))
        .with_ansi(std::io::stdout().is_terminal()) // plain text in the journal
        .init();
    let cfg = config::Config::load()?;
    std::fs::create_dir_all(cfg.state_dir.join("images")).context("creating the state directory")?;
    let agent = Arc::new(agent::Agent::new(cfg.clone())?);
    let app = api::router(agent);

    if let Some(path) = cfg.listen.strip_prefix("unix:") {
        // Only agentd and tailnet-proxy (via the socket's group) can connect;
        // the shell user, which runs the agent's commands, can't.
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!("another agentd is already listening on {path}");
        }
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path).with_context(|| format!("binding {path}"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
        tracing::info!(socket = path, model = %cfg.model, "listening");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(&cfg.listen)
            .await
            .with_context(|| format!("binding {}", cfg.listen))?;
        tracing::info!(address = %cfg.listen, model = %cfg.model, "listening");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown())
            .await?;
    }
    Ok(())
}

async fn shutdown() {
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}
