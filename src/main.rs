mod config;
mod firecracker;
mod github;
mod metrics;
mod netlink;
mod orchestrator;
mod pool;
mod server;
mod setup;

use std::sync::Arc;

use anyhow::Context;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("fc_runner=info".parse()?),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/etc/fc-runner/config.toml".into());

    tracing::info!(path = %config_path, "loading configuration");
    let mut config =
        config::AppConfig::load(&config_path).context("failed to load configuration")?;

    // Download kernel / build golden rootfs / resolve network allowlists if missing
    setup::ensure_vm_assets(&mut config)
        .await
        .context("failed to provision VM assets")?;

    let config = Arc::new(config);

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("received shutdown signal");
        cancel_clone.cancel();
    });

    // Start management HTTP server if enabled
    let server_state = Arc::new(server::ServerState::new(&config.server));
    if config.server.enabled {
        let listen_addr: std::net::SocketAddr = config
            .server
            .listen_addr
            .parse()
            .context("invalid server.listen_addr")?;
        let state = server_state.clone();
        tokio::spawn(async move {
            if let Err(e) = server::start(listen_addr, state).await {
                tracing::error!(error = %e, "management server failed");
            }
        });
    }

    // Initialize metrics with initial slot count
    metrics::POOL_SLOTS_AVAILABLE.set(config.runner.max_concurrent_jobs as i64);

    let orchestrator = orchestrator::Orchestrator::new(config, cancel, server_state)?;
    orchestrator.run().await?;

    tracing::info!("fc-runner exiting");
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
}
