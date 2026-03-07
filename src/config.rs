use anyhow::{bail, Context};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub github: GitHubConfig,
    pub firecracker: FirecrackerConfig,
    pub runner: RunnerConfig,
    #[serde(default)]
    pub network: NetworkConfig,
}

#[derive(Clone, Deserialize)]
pub struct GitHubConfig {
    pub token: SecretString,
    pub owner: String,
    /// Single repo (backward-compatible). Use `repos` for multiple.
    #[serde(default)]
    pub repo: Option<String>,
    /// List of repos under the same owner. If both `repo` and `repos` are set,
    /// they are merged (deduplicated).
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default = "default_runner_group_id")]
    pub runner_group_id: u64,
    #[serde(default = "default_labels")]
    pub labels: Vec<String>,
}

impl GitHubConfig {
    /// Returns the deduplicated list of repos to poll.
    pub fn all_repos(&self) -> Vec<String> {
        let mut repos = self.repos.clone();
        if let Some(ref r) = self.repo {
            if !r.is_empty() && !repos.contains(r) {
                repos.insert(0, r.clone());
            }
        }
        repos
    }
}

impl std::fmt::Debug for GitHubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubConfig")
            .field("owner", &self.owner)
            .field("repos", &self.all_repos())
            .field("runner_group_id", &self.runner_group_id)
            .field("labels", &self.labels)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FirecrackerConfig {
    #[serde(default = "default_binary_path")]
    pub binary_path: String,
    pub kernel_path: String,
    pub rootfs_golden: String,
    #[serde(default = "default_vcpu_count")]
    pub vcpu_count: u32,
    #[serde(default = "default_mem_size_mib")]
    pub mem_size_mib: u32,
    pub vm_config_template: String,
    /// Path to the jailer binary. When set, VMs run inside a jailer chroot
    /// with seccomp-BPF and dropped privileges.
    pub jailer_path: Option<String>,
    /// UID the jailer drops to (required when jailer_path is set)
    pub jailer_uid: Option<u32>,
    /// GID the jailer drops to (required when jailer_path is set)
    pub jailer_gid: Option<u32>,
    /// Chroot base directory for jailer (default: /srv/jailer)
    #[serde(default = "default_jailer_chroot_base")]
    pub jailer_chroot_base: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunnerConfig {
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_max_concurrent_jobs")]
    pub max_concurrent_jobs: usize,
    #[serde(default = "default_vm_timeout_secs")]
    pub vm_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    #[serde(default = "default_host_ip")]
    pub host_ip: String,
    #[serde(default = "default_guest_ip")]
    pub guest_ip: String,
    #[serde(default = "default_cidr")]
    pub cidr: String,
    #[serde(default = "default_dns")]
    pub dns: Vec<String>,
    /// Allowed outbound network CIDRs for guest VMs.
    /// Use "github" as a magic keyword to auto-fetch GitHub Actions CIDRs
    /// from https://api.github.com/meta at startup.
    /// When empty (default), all outbound traffic is allowed.
    /// Example: ["github", "10.0.0.0/8", "192.168.1.0/24"]
    #[serde(default)]
    pub allowed_networks: Vec<String>,
    /// Resolved CIDRs after expanding "github" keyword (populated at runtime)
    #[serde(skip)]
    pub resolved_networks: Vec<String>,
}

fn default_runner_group_id() -> u64 {
    1
}

fn default_labels() -> Vec<String> {
    vec![
        "self-hosted".into(),
        "linux".into(),
        "firecracker".into(),
    ]
}

fn default_binary_path() -> String {
    "/usr/local/bin/firecracker".into()
}

fn default_vcpu_count() -> u32 {
    2
}

fn default_mem_size_mib() -> u32 {
    2048
}

fn default_jailer_chroot_base() -> String {
    "/srv/jailer".into()
}

fn default_work_dir() -> String {
    "/var/lib/fc-runner/vms".into()
}

fn default_poll_interval() -> u64 {
    5
}

fn default_max_concurrent_jobs() -> usize {
    4
}

fn default_vm_timeout_secs() -> u64 {
    3600
}

fn default_host_ip() -> String {
    "172.16.0.1".into()
}

fn default_guest_ip() -> String {
    "172.16.0.2".into()
}

fn default_cidr() -> String {
    "24".into()
}

fn default_dns() -> Vec<String> {
    vec!["8.8.8.8".into(), "1.1.1.1".into()]
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            host_ip: default_host_ip(),
            guest_ip: default_guest_ip(),
            cidr: default_cidr(),
            dns: default_dns(),
            allowed_networks: Vec::new(),
            resolved_networks: Vec::new(),
        }
    }
}

/// Check that a path is not a symlink (prevents symlink-based attacks).
fn reject_symlink(label: &str, path: &Path) -> anyhow::Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            bail!(
                "{} is a symlink (security risk): {}",
                label,
                path.display()
            );
        }
    }
    Ok(())
}

impl AppConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        // Validate config file permissions (should not be world-readable)
        let config_path = Path::new(path);
        if config_path.exists() {
            let meta = std::fs::metadata(config_path)
                .with_context(|| format!("reading config metadata: {}", path))?;
            let mode = meta.mode() & 0o777;
            if mode & 0o007 != 0 {
                bail!(
                    "config file is world-readable (mode {:o}) — contains secrets!\n\
                     Fix with: chmod 600 {}",
                    mode, path
                );
            }
        }

        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading config: {}", path))?;
        let config: AppConfig =
            toml::from_str(&content).with_context(|| "parsing config TOML")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.github.token.expose_secret().is_empty() {
            bail!("github.token must not be empty");
        }
        if self.github.owner.is_empty() {
            bail!("github.owner must not be empty");
        }
        if self.github.all_repos().is_empty() {
            bail!("at least one repo must be configured (set github.repo or github.repos)");
        }
        if self.firecracker.vcpu_count == 0 {
            bail!("firecracker.vcpu_count must be > 0");
        }
        if self.firecracker.mem_size_mib == 0 {
            bail!("firecracker.mem_size_mib must be > 0");
        }
        if self.runner.max_concurrent_jobs == 0 {
            bail!("runner.max_concurrent_jobs must be > 0");
        }

        // kernel_path and rootfs_golden are auto-provisioned by setup::ensure_vm_assets
        let required_paths = [
            ("firecracker.binary_path", &self.firecracker.binary_path),
            (
                "firecracker.vm_config_template",
                &self.firecracker.vm_config_template,
            ),
        ];
        for (name, path) in required_paths {
            let p = Path::new(path);
            reject_symlink(name, p)?;
            if !p.exists() {
                bail!("{} does not exist: {}", name, path);
            }
        }

        // Validate jailer configuration
        if let Some(jailer_path) = &self.firecracker.jailer_path {
            let p = Path::new(jailer_path);
            reject_symlink("firecracker.jailer_path", p)?;
            if !p.exists() {
                bail!("firecracker.jailer_path does not exist: {}", jailer_path);
            }
            if self.firecracker.jailer_uid.is_none() {
                bail!("firecracker.jailer_uid is required when jailer_path is set");
            }
            if self.firecracker.jailer_gid.is_none() {
                bail!("firecracker.jailer_gid is required when jailer_path is set");
            }
        }

        // Validate work_dir is not a symlink and set restrictive permissions
        let work_dir = Path::new(&self.runner.work_dir);
        reject_symlink("runner.work_dir", work_dir)?;
        if !work_dir.exists() {
            std::fs::create_dir_all(work_dir)
                .with_context(|| format!("creating work_dir: {}", self.runner.work_dir))?;
        }
        // Try to restrict work_dir to owner only (contains VM rootfs with tokens).
        // This may fail under systemd ProtectSystem=strict — that's fine since
        // systemd already restricts access.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            if let Err(e) = std::fs::set_permissions(work_dir, perms) {
                tracing::warn!(
                    path = %self.runner.work_dir,
                    error = %e,
                    "could not set work_dir permissions to 0700 (ok if running under systemd ProtectSystem)"
                );
            }
        }

        Ok(())
    }
}
