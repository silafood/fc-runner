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
    pub repo: String,
    #[serde(default = "default_runner_group_id")]
    pub runner_group_id: u64,
    #[serde(default = "default_labels")]
    pub labels: Vec<String>,
}

impl std::fmt::Debug for GitHubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubConfig")
            .field("owner", &self.owner)
            .field("repo", &self.repo)
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
    #[serde(default = "default_tap_interface")]
    pub tap_interface: String,
    pub vm_config_template: String,
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
    #[serde(default = "default_tap_device")]
    pub tap_device: String,
    #[serde(default = "default_host_ip")]
    pub host_ip: String,
    #[serde(default = "default_guest_ip")]
    pub guest_ip: String,
    #[serde(default = "default_cidr")]
    pub cidr: String,
    #[serde(default = "default_dns")]
    pub dns: Vec<String>,
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

fn default_tap_interface() -> String {
    "tap-fc0".into()
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

fn default_tap_device() -> String {
    "tap-fc0".into()
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
            tap_device: default_tap_device(),
            host_ip: default_host_ip(),
            guest_ip: default_guest_ip(),
            cidr: default_cidr(),
            dns: default_dns(),
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
                tracing::warn!(
                    path = path,
                    mode = format!("{:o}", mode),
                    "config file is world-readable — contains secrets! Fix with: chmod 640 {}",
                    path
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
        if self.github.repo.is_empty() {
            bail!("github.repo must not be empty");
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

        // Validate work_dir is not a symlink and set restrictive permissions
        let work_dir = Path::new(&self.runner.work_dir);
        reject_symlink("runner.work_dir", work_dir)?;
        if !work_dir.exists() {
            std::fs::create_dir_all(work_dir)
                .with_context(|| format!("creating work_dir: {}", self.runner.work_dir))?;
        }
        // Restrict work_dir to owner only (contains VM rootfs with tokens)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(work_dir, perms)
                .with_context(|| format!("setting work_dir permissions: {}", self.runner.work_dir))?;
        }

        Ok(())
    }
}
