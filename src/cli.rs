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
        #[arg(
            short = 'f',
            long = "config",
            default_value = "/etc/fc-runner/config.toml"
        )]
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
        #[arg(
            short = 'f',
            long = "config",
            default_value = "/etc/fc-runner/config.toml"
        )]
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

    /// Stream logs from VMs (SSE)
    Logs {
        /// Server endpoint
        #[arg(short, long, default_value = "http://localhost:9090")]
        endpoint: String,

        /// VM ID to filter logs (omit for all VMs)
        #[arg(long)]
        vm_id: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_parse_server() {
        let cli = Cli::parse_from(["fc-runner", "server", "--config", "/path/to/config.toml"]);
        match cli.command {
            Commands::Server { config } => assert_eq!(config, "/path/to/config.toml"),
            _ => panic!("expected Server command"),
        }
    }

    #[test]
    fn cli_server_default_config() {
        let cli = Cli::parse_from(["fc-runner", "server"]);
        match cli.command {
            Commands::Server { config } => assert_eq!(config, "/etc/fc-runner/config.toml"),
            _ => panic!("expected Server command"),
        }
    }

    #[test]
    fn cli_server_short_flag() {
        let cli = Cli::parse_from(["fc-runner", "server", "-f", "/tmp/config.toml"]);
        match cli.command {
            Commands::Server { config } => assert_eq!(config, "/tmp/config.toml"),
            _ => panic!("expected Server command"),
        }
    }

    #[test]
    fn cli_parse_agent() {
        let cli = Cli::parse_from(["fc-runner", "agent", "--log-level", "debug"]);
        match cli.command {
            Commands::Agent { log_level } => assert_eq!(log_level, "debug"),
            _ => panic!("expected Agent command"),
        }
    }

    #[test]
    fn cli_agent_default_log_level() {
        let cli = Cli::parse_from(["fc-runner", "agent"]);
        match cli.command {
            Commands::Agent { log_level } => assert_eq!(log_level, "info"),
            _ => panic!("expected Agent command"),
        }
    }

    #[test]
    fn cli_parse_validate() {
        let cli = Cli::parse_from(["fc-runner", "validate", "--config", "/tmp/config.toml"]);
        match cli.command {
            Commands::Validate { config } => assert_eq!(config, "/tmp/config.toml"),
            _ => panic!("expected Validate command"),
        }
    }

    #[test]
    fn cli_validate_default_config() {
        let cli = Cli::parse_from(["fc-runner", "validate"]);
        match cli.command {
            Commands::Validate { config } => assert_eq!(config, "/etc/fc-runner/config.toml"),
            _ => panic!("expected Validate command"),
        }
    }

    #[test]
    fn cli_parse_ps() {
        let cli = Cli::parse_from(["fc-runner", "ps", "--endpoint", "http://host:8080"]);
        match cli.command {
            Commands::Ps { endpoint } => assert_eq!(endpoint, "http://host:8080"),
            _ => panic!("expected Ps command"),
        }
    }

    #[test]
    fn cli_ps_default_endpoint() {
        let cli = Cli::parse_from(["fc-runner", "ps"]);
        match cli.command {
            Commands::Ps { endpoint } => assert_eq!(endpoint, "http://localhost:9090"),
            _ => panic!("expected Ps command"),
        }
    }

    #[test]
    fn cli_parse_pools_list() {
        let cli = Cli::parse_from(["fc-runner", "pools", "list"]);
        match cli.command {
            Commands::Pools { action } => match action {
                PoolAction::List { endpoint } => {
                    assert_eq!(endpoint, "http://localhost:9090");
                }
                _ => panic!("expected List action"),
            },
            _ => panic!("expected Pools command"),
        }
    }

    #[test]
    fn cli_parse_pools_scale() {
        let cli = Cli::parse_from([
            "fc-runner",
            "pools",
            "scale",
            "default",
            "--min-ready",
            "3",
            "--max-ready",
            "8",
            "--endpoint",
            "http://host:8080",
        ]);
        match cli.command {
            Commands::Pools { action } => match action {
                PoolAction::Scale {
                    name,
                    min_ready,
                    max_ready,
                    endpoint,
                } => {
                    assert_eq!(name, "default");
                    assert_eq!(min_ready, Some(3));
                    assert_eq!(max_ready, Some(8));
                    assert_eq!(endpoint, "http://host:8080");
                }
                _ => panic!("expected Scale action"),
            },
            _ => panic!("expected Pools command"),
        }
    }

    #[test]
    fn cli_parse_pools_scale_partial() {
        let cli = Cli::parse_from(["fc-runner", "pools", "scale", "heavy", "--min-ready", "5"]);
        match cli.command {
            Commands::Pools { action } => match action {
                PoolAction::Scale {
                    name,
                    min_ready,
                    max_ready,
                    ..
                } => {
                    assert_eq!(name, "heavy");
                    assert_eq!(min_ready, Some(5));
                    assert_eq!(max_ready, None);
                }
                _ => panic!("expected Scale action"),
            },
            _ => panic!("expected Pools command"),
        }
    }

    #[test]
    fn cli_parse_pools_pause() {
        let cli = Cli::parse_from(["fc-runner", "pools", "pause", "default"]);
        match cli.command {
            Commands::Pools { action } => match action {
                PoolAction::Pause { name, .. } => assert_eq!(name, "default"),
                _ => panic!("expected Pause action"),
            },
            _ => panic!("expected Pools command"),
        }
    }

    #[test]
    fn cli_parse_pools_resume() {
        let cli = Cli::parse_from(["fc-runner", "pools", "resume", "default"]);
        match cli.command {
            Commands::Pools { action } => match action {
                PoolAction::Resume { name, .. } => assert_eq!(name, "default"),
                _ => panic!("expected Resume action"),
            },
            _ => panic!("expected Pools command"),
        }
    }

    #[test]
    fn cli_parse_logs_with_vm_id() {
        let cli = Cli::parse_from(["fc-runner", "logs", "--vm-id", "fc-123-slot0"]);
        match cli.command {
            Commands::Logs { vm_id, endpoint } => {
                assert_eq!(vm_id, Some("fc-123-slot0".to_string()));
                assert_eq!(endpoint, "http://localhost:9090");
            }
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn cli_logs_all_vms() {
        let cli = Cli::parse_from(["fc-runner", "logs"]);
        match cli.command {
            Commands::Logs { vm_id, .. } => assert_eq!(vm_id, None),
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn cli_missing_subcommand_fails() {
        let result = Cli::try_parse_from(["fc-runner"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_invalid_subcommand_fails() {
        let result = Cli::try_parse_from(["fc-runner", "nonexistent"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_pools_scale_requires_name() {
        let result = Cli::try_parse_from(["fc-runner", "pools", "scale"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_pools_pause_requires_name() {
        let result = Cli::try_parse_from(["fc-runner", "pools", "pause"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_logs_no_vm_id_is_valid() {
        let result = Cli::try_parse_from(["fc-runner", "logs"]);
        assert!(result.is_ok());
    }
}
