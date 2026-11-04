//! agentd: the agent on a cloud desktop. Runs the model loop with browser and
//! shell tools, keeps the chat history, and serves the chat API to the apps.

mod agent;
mod api;
mod attest;
mod aws;
mod browser;
mod cdp;
mod clock;
mod config;
mod consolidate;
mod destinations;
mod effects;
mod egress;
mod gateway;
mod guard;
mod llm;
mod memory;
mod monitoring;
mod policy;
mod push;
mod schedule;
mod secrets;
mod shell;
mod skills;
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
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--check-gateway")
    {
        return check_gateway().await;
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("agentd=info")))
        .with_ansi(std::io::stdout().is_terminal()) // plain text in the journal
        .init();
    let cfg = config::Config::load()?;
    std::fs::create_dir_all(cfg.state_dir.join("images")).context("creating the state directory")?;
    let agent = Arc::new(agent::Agent::new(cfg.clone())?);
    let app = api::router(agent.clone());
    start_egress_gate(&cfg, &agent).await;

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
        // Only once this agentd owns the socket: never two schedulers or
        // effect runners.
        tokio::spawn(schedule::run(agent.clone()));
        tokio::spawn(effects::run(agent));
        serve_until_stopped(axum::serve(listener, app).with_graceful_shutdown(shutdown())).await?;
    } else {
        let listener = tokio::net::TcpListener::bind(&cfg.listen)
            .await
            .with_context(|| format!("binding {}", cfg.listen))?;
        tracing::info!(address = %cfg.listen, model = %cfg.model, "listening");
        tokio::spawn(schedule::run(agent.clone()));
        tokio::spawn(effects::run(agent));
        serve_until_stopped(axum::serve(listener, app).with_graceful_shutdown(shutdown())).await?;
    }
    Ok(())
}

/// Serves until SIGTERM, then gives open requests a few seconds. The apps'
/// event streams never end on their own: without a limit, every restart would
/// hang until systemd kills agentd (90 s), with the apps cut off meanwhile.
async fn serve_until_stopped(
    server: impl std::future::IntoFuture<Output = std::io::Result<()>>,
) -> Result<()> {
    tokio::select! {
        result = server.into_future() => result?,
        () = async {
            shutdown().await;
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        } => tracing::info!("stopping with event streams still open"),
    }
    Ok(())
}

/// `agentd --check-gateway` (as the agentd user): checks the model gateway
/// the way a model call would, without sending a prompt.
async fn check_gateway() -> Result<()> {
    let cfg = config::Config::load()?;
    anyhow::ensure!(!cfg.gateway_url.is_empty(), "GATEWAY_URL isn't set");
    let gateway = gateway::ModelGateway::new(
        &cfg.gateway_url,
        &cfg.aws_region,
        &cfg.gateway_audience,
        cfg.gateway_trust,
        std::time::Duration::from_secs(30),
    )?;
    println!("{}: {}", cfg.gateway_url, gateway.check().await?);
    Ok(())
}

async fn shutdown() {
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

/// The egress gate, scheduled runs' way out (egress.rs). If it can't start,
/// scheduled commands reach nothing (Squid is closed to them while they run)
/// and their reports say so.
async fn start_egress_gate(cfg: &config::Config, agent: &Arc<agent::Agent>) {
    // A lock left by an agentd that stopped mid-command.
    agent.clear_egress_locks();
    if cfg.egress_gate_listen.is_empty() {
        return;
    }
    let weak = Arc::downgrade(agent);
    let keys: egress::Keys = Box::new(move |name| weak.upgrade()?.secrets.get(name));
    let started = async {
        let gate = Arc::new(egress::EgressGate::new(
            keys,
            &cfg.egress_gate_squid,
            cfg.egress_gate_upstream.clone(),
        )?);
        gate.write_bundle(&cfg.state_dir.join(agent::EGRESS_BUNDLE))?;
        let listener = tokio::net::TcpListener::bind(&cfg.egress_gate_listen)
            .await
            .with_context(|| format!("binding {}", cfg.egress_gate_listen))?;
        tokio::spawn(gate.clone().serve(listener));
        anyhow::Ok(gate)
    };
    match started.await {
        Ok(gate) => {
            let _ = agent.gate.set(gate);
            tracing::info!(listen = %cfg.egress_gate_listen, "egress gate for scheduled runs");
        }
        Err(err) => tracing::error!(err = %format!("{err:#}"), "the egress gate didn't start"),
    }
}
