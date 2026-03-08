use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fc-runner", about = "Firecracker-based GitHub Actions runner")]
#[command(version, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the fc-runner server (orchestrator + management API)
    Server {
        /// Path to the configuration file
        #[arg(short = 'f', long = "config", default_value = "/etc/fc-runner/config.toml")]
        config: String,
    },

    /// Start the guest agent inside a Firecracker VM
    Agent {
        /// Log level (trace, debug, info, warn, error)
        #[arg(short, long, default_value = "info")]
        log_level: String,
    },

    /// Validate a configuration file without starting the server
    Validate {
        /// Path to the configuration file
        #[arg(short = 'f', long = "config", default_value = "/etc/fc-runner/config.toml")]
        config: String,
    },

    /// List running VMs
    Ps {
        /// Server endpoint
        #[arg(short, long, default_value = "http://localhost:9090")]
        endpoint: String,
    },

    /// Pool management commands
    Pools {
        #[command(subcommand)]
        action: PoolAction,
    },

    /// Stream logs from a VM
    Logs {
        /// Server endpoint
        #[arg(short, long, default_value = "http://localhost:9090")]
        endpoint: String,

        /// VM ID to stream logs from
        #[arg(long)]
        vm_id: String,

        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
}

#[derive(Subcommand)]
pub enum PoolAction {
    /// List all pools
    List {
        /// Server endpoint
        #[arg(short, long, default_value = "http://localhost:9090")]
        endpoint: String,
    },

    /// Scale a pool
    Scale {
        /// Pool name
        name: String,

        /// Minimum ready VMs
        #[arg(long)]
        min_ready: Option<usize>,

        /// Maximum ready VMs
        #[arg(long)]
        max_ready: Option<usize>,

        /// Server endpoint
        #[arg(short, long, default_value = "http://localhost:9090")]
        endpoint: String,
    },

    /// Pause a pool (stop creating new VMs)
    Pause {
        /// Pool name
        name: String,

        /// Server endpoint
        #[arg(short, long, default_value = "http://localhost:9090")]
        endpoint: String,
    },

    /// Resume a paused pool
    Resume {
        /// Pool name
        name: String,

        /// Server endpoint
        #[arg(short, long, default_value = "http://localhost:9090")]
        endpoint: String,
    },
}
