//! Standalone guest agent binary for Firecracker VMs.
//!
//! This is a lightweight entry point that only pulls in the agent module,
//! avoiding the full host-side dependency tree (axum, firecracker-rs-sdk,
//! aws-sdk-s3, etc.).

use clap::Parser;

#[derive(Parser)]
#[command(name = "fc-runner-agent", about = "Firecracker VM guest agent")]
struct Args {
    /// Log level (error, warn, info, debug, trace)
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    fc_runner::agent::run(&args.log_level).await
}
