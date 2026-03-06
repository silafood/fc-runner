mod config;
mod firecracker;
mod github;
mod orchestrator;
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

    let orchestrator = orchestrator::Orchestrator::new(config, cancel)?;
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
