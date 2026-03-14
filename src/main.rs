mod agent;
mod api_client;
mod cache_server;
mod cli;
mod config;
mod firecracker;
mod github;
mod image;
mod metrics;
mod netlink;
mod orchestrator;
mod pool;
mod server;
mod setup;
mod version;
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

    tracing::info!(version = %version::version(), "fc-runner starting");
    tracing::info!(path = %config_path, "loading configuration");
    let mut config =
        config::AppConfig::load(config_path).context("failed to load configuration")?;

    setup::ensure_vm_assets(&mut config)
        .await
        .context("failed to provision VM assets")?;

    // Resolve cache service token before freezing config in Arc.
    // If no token is configured, generate a random one so that both the
    // HTTP server and the VM provisioning code share the same value.
    if config.cache_service.enabled && config.cache_service.token.is_none() {
        config.cache_service.token = Some(uuid::Uuid::new_v4().to_string());
    }

    let config = Arc::new(config);

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("received shutdown signal");
        cancel_clone.cancel();
    });

    // Initialize cache service if enabled
    let cache_state = if config.cache_service.enabled {
        let token = config.cache_service.token.clone().unwrap_or_default();
        let cs = cache_server::CacheState::new(&config.cache_service, token)
            .await
            .context("failed to initialize cache service")?;
        Some(cs)
    } else {
        None
    };

    let server_state = Arc::new(server::ServerState::new(&config.server));
    if config.server.enabled {
        let listen_addr: std::net::SocketAddr = config
            .server
            .listen_addr
            .parse()
            .context("invalid server.listen_addr")?;
        let state = server_state.clone();
        let cs = cache_state.clone();
        tokio::spawn(async move {
            if let Err(e) = server::start(listen_addr, state, cs).await {
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
            println!(
                "  vcpu: {}, mem: {} MiB",
                config.firecracker.vcpu_count, config.firecracker.mem_size_mib
            );
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
        "{:<38} {:<12} {:<30} {:<6} STARTED",
        "VM ID", "JOB ID", "REPO", "SLOT"
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
            let pools = client.list_pools().await?;

            if pools.is_empty() {
                println!("no pools configured");
                return Ok(());
            }

            println!(
                "{:<20} {:<8} {:<10} {:<10} {:<8} {:<8} REPOS",
                "NAME", "PAUSED", "MIN_READY", "MAX_READY", "ACTIVE", "IDLE"
            );
            for p in &pools {
                println!(
                    "{:<20} {:<8} {:<10} {:<10} {:<8} {:<8} {:?}",
                    p.name,
                    if p.paused { "yes" } else { "no" },
                    p.min_ready,
                    p.max_ready,
                    p.active,
                    p.idle_slots,
                    p.repos
                );
            }
            Ok(())
        }
        PoolAction::Scale {
            name,
            min_ready,
            max_ready,
            endpoint,
        } => {
            if min_ready.is_none() && max_ready.is_none() {
                anyhow::bail!("at least one of --min-ready or --max-ready must be specified");
            }
            let client = api_client::ApiClient::new(&endpoint);
            let resp = client.scale_pool(&name, min_ready, max_ready).await?;
            println!("{}", resp.message);
            Ok(())
        }
        PoolAction::Pause { name, endpoint } => {
            let client = api_client::ApiClient::new(&endpoint);
            let resp = client.pause_pool(&name).await?;
            println!("{}", resp.message);
            Ok(())
        }
        PoolAction::Resume { name, endpoint } => {
            let client = api_client::ApiClient::new(&endpoint);
            let resp = client.resume_pool(&name).await?;
            println!("{}", resp.message);
            Ok(())
        }
    }
}

async fn run_logs(endpoint: &str, vm_id: &str, follow: bool) -> anyhow::Result<()> {
    use futures_util::StreamExt;

    let client = api_client::ApiClient::new(endpoint);
    let vm_filter = if vm_id.is_empty() { None } else { Some(vm_id) };

    println!(
        "streaming logs{} from {} (Ctrl+C to stop)",
        vm_filter
            .map(|id| format!(" for VM {}", id))
            .unwrap_or_default(),
        endpoint
    );

    let resp = client.stream_logs(vm_filter).await?;
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("error reading SSE stream")?;
        let text = String::from_utf8_lossy(&chunk);
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data: ")
                && let Ok(event) = serde_json::from_str::<serde_json::Value>(data)
            {
                let vm = event["vm_id"].as_str().unwrap_or("?");
                let etype = event["event_type"].as_str().unwrap_or("?");
                let msg = event["message"].as_str().unwrap_or("");
                let ts = event["timestamp"].as_str().unwrap_or("");
                println!("[{}] {} [{}] {}", ts, vm, etype, msg);
            }
        }
        if !follow {
            break;
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
}
