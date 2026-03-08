mod agent;
mod api_client;
mod cli;
mod config;
mod firecracker;
mod github;
mod metrics;
mod netlink;
mod orchestrator;
mod pool;
mod server;
mod setup;
mod vsock;

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;

use cli::{Cli, Commands, PoolAction};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server { config } => run_server(&config).await,
        Commands::Agent { log_level } => run_agent(&log_level).await,
        Commands::Validate { config } => run_validate(&config),
        Commands::Ps { endpoint } => run_ps(&endpoint).await,
        Commands::Pools { action } => run_pools(action).await,
        Commands::Logs {
            endpoint,
            vm_id,
            follow,
        } => run_logs(&endpoint, &vm_id, follow).await,
    }
}

async fn run_server(config_path: &str) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("fc_runner=info".parse()?),
        )
        .init();

    tracing::info!(path = %config_path, "loading configuration");
    let mut config =
        config::AppConfig::load(config_path).context("failed to load configuration")?;

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

    metrics::POOL_SLOTS_AVAILABLE.set(config.runner.max_concurrent_jobs as i64);

    let orchestrator = orchestrator::Orchestrator::new(config, cancel, server_state)?;
    orchestrator.run().await?;

    tracing::info!("fc-runner exiting");
    Ok(())
}

async fn run_agent(log_level: &str) -> anyhow::Result<()> {
    agent::run(log_level).await
}

fn run_validate(config_path: &str) -> anyhow::Result<()> {
    match config::AppConfig::load(config_path) {
        Ok(config) => {
            println!("configuration is valid");
            println!("  owner: {}", config.github.owner);
            println!("  repos: {:?}", config.github.all_repos());
            println!(
                "  max_concurrent_jobs: {}",
                config.runner.max_concurrent_jobs
            );
            println!("  vcpu: {}, mem: {} MiB", config.firecracker.vcpu_count, config.firecracker.mem_size_mib);
            if !config.pool.is_empty() {
                println!("  pools:");
                for p in &config.pool {
                    println!(
                        "    - {} (repos: {:?}, min_ready: {}, max_ready: {})",
                        p.name, p.repos, p.min_ready, p.max_ready
                    );
                }
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("configuration error: {:#}", e);
            std::process::exit(1);
        }
    }
}

async fn run_ps(endpoint: &str) -> anyhow::Result<()> {
    let client = api_client::ApiClient::new(endpoint);
    let vms = client.list_vms().await?;

    if vms.is_empty() {
        println!("no running VMs");
        return Ok(());
    }

    println!(
        "{:<38} {:<12} {:<30} {:<6} {}",
        "VM ID", "JOB ID", "REPO", "SLOT", "STARTED"
    );
    for vm in &vms {
        println!(
            "{:<38} {:<12} {:<30} {:<6} {}",
            vm.vm_id, vm.job_id, vm.repo, vm.slot, vm.started_at
        );
    }
    Ok(())
}

async fn run_pools(action: PoolAction) -> anyhow::Result<()> {
    match action {
        PoolAction::List { endpoint } => {
            let client = api_client::ApiClient::new(&endpoint);
            let status = client.status().await?;
            println!("server: {} (v{}, uptime: {}s)", endpoint, status.version, status.uptime_seconds);
            println!("mode: {}, active VMs: {}", status.mode, status.active_vms);
            println!("\npool management endpoints will be available after pool management API is implemented");
            Ok(())
        }
        PoolAction::Scale {
            name,
            min_ready,
            max_ready,
            endpoint,
        } => {
            println!(
                "scale pool '{}' at {} (min_ready: {:?}, max_ready: {:?}) — not yet implemented",
                name, endpoint, min_ready, max_ready
            );
            Ok(())
        }
        PoolAction::Pause { name, endpoint } => {
            println!(
                "pause pool '{}' at {} — not yet implemented",
                name, endpoint
            );
            Ok(())
        }
        PoolAction::Resume { name, endpoint } => {
            println!(
                "resume pool '{}' at {} — not yet implemented",
                name, endpoint
            );
            Ok(())
        }
    }
}

async fn run_logs(endpoint: &str, vm_id: &str, follow: bool) -> anyhow::Result<()> {
    println!(
        "logs for VM {} at {} (follow: {}) — not yet implemented",
        vm_id, endpoint, follow
    );
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
