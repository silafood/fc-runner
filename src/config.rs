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
    #[serde(default)]
    pub server: ServerConfig,
    /// Named VM pools with per-pool repos, replica counts, and resource overrides.
    /// When configured, pools replace the flat warm_pool_size setting.
    #[serde(default)]
    pub pool: Vec<PoolConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_server_enabled")]
    pub enabled: bool,
    /// Optional API key for management endpoints (if unset, no auth required)
    pub api_key: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen_addr(),
            enabled: default_server_enabled(),
            api_key: None,
        }
    }
}

fn default_listen_addr() -> String {
    "0.0.0.0:9090".into()
}

fn default_server_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolConfig {
    pub name: String,
    /// Repos this pool serves (must be subset of github.repos).
    pub repos: Vec<String>,
    /// Minimum number of idle VMs to keep ready.
    #[serde(default = "default_min_ready")]
    pub min_ready: usize,
    /// Maximum total VMs in this pool (idle + active).
    #[serde(default = "default_max_ready")]
    pub max_ready: usize,
    /// Override vcpu_count for this pool (defaults to firecracker.vcpu_count).
    pub vcpu_count: Option<u32>,
    /// Override mem_size_mib for this pool (defaults to firecracker.mem_size_mib).
    pub mem_size_mib: Option<u32>,
}

fn default_min_ready() -> usize {
    1
}

fn default_max_ready() -> usize {
    4
}

#[derive(Clone, Deserialize)]
pub struct GitHubConfig {
    /// PAT token (required unless [github.app] is configured)
    #[serde(default)]
    pub token: Option<SecretString>,
    pub owner: String,
    /// Organization name for org-level runners (optional).
    /// When set, runners register at the org level and can pick up jobs
    /// from any repo in the organization.
    #[serde(default)]
    pub organization: Option<String>,
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
    /// GitHub App authentication (alternative to PAT)
    pub app: Option<GitHubAppConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubAppConfig {
    pub app_id: u64,
    pub installation_id: u64,
    pub private_key_path: String,
}

impl GitHubConfig {
    /// Returns the deduplicated list of repos to poll.
    pub fn all_repos(&self) -> Vec<String> {
        let mut repos = self.repos.clone();
        if let Some(ref r) = self.repo
            && !r.is_empty()
            && !repos.contains(r)
        {
            repos.insert(0, r.clone());
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
    #[serde(default = "default_boot_args")]
    pub boot_args: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
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
    /// URL for the cloud image used to build the golden rootfs (optional, has default)
    pub cloud_img_url: Option<String>,
    /// Secret injection method: "mmds" (default) uses Firecracker's MMDS service,
    /// "mount" uses the legacy loop-mount injection into the rootfs.
    #[serde(default = "default_secret_injection")]
    pub secret_injection: String,
    /// Enable VSOCK device for guest agent communication.
    /// When enabled, each VM gets a VSOCK device with CID = vsock_cid_base + slot.
    #[serde(default)]
    pub vsock_enabled: bool,
    /// Base CID for VSOCK devices (default: 3, since CID 1 and 2 are reserved).
    #[serde(default = "default_vsock_cid_base")]
    pub vsock_cid_base: u32,
}

fn default_secret_injection() -> String {
    "mmds".into()
}

fn default_vsock_cid_base() -> u32 {
    3
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
    /// Number of idle VMs to maintain in a warm pool.
    /// When > 0, uses registration tokens to pre-register runners that wait
    /// for GitHub to assign jobs. Replaced as they finish. Default: 0 (JIT mode).
    #[serde(default)]
    pub warm_pool_size: usize,
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

fn default_boot_args() -> String {
    "console=ttyS0 reboot=k panic=1 pci=off fsck.mode=skip quiet loglevel=3".into()
}

fn default_log_level() -> String {
    "Warning".into()
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
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        bail!(
            "{} is a symlink (security risk): {}",
            label,
            path.display()
        );
    }
    Ok(())
}

impl AppConfig {
    /// Parse config from a TOML string (for testing).
    #[cfg(test)]
    pub fn from_str(content: &str) -> anyhow::Result<Self> {
        let config: AppConfig =
            toml::from_str(content).with_context(|| "parsing config TOML")?;
        Ok(config)
    }

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
        let has_token = self
            .github
            .token
            .as_ref()
            .map(|t| !t.expose_secret().is_empty())
            .unwrap_or(false);
        let has_app = self.github.app.is_some();
        if !has_token && !has_app {
            bail!(
                "either github.token or [github.app] must be configured"
            );
        }
        if let Some(app) = &self.github.app {
            let key_path = Path::new(&app.private_key_path);
            reject_symlink("github.app.private_key_path", key_path)?;
            if !key_path.exists() {
                bail!(
                    "github.app.private_key_path does not exist: {}",
                    app.private_key_path
                );
            }
        }
        if self.github.owner.is_empty() {
            bail!("github.owner must not be empty");
        }
        if self.github.all_repos().is_empty() && self.github.organization.is_none() {
            bail!("at least one repo must be configured (set github.repo or github.repos), or set github.organization for org-level runners");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config(github_extra: &str) -> String {
        format!(
            r#"
[github]
token = "ghp_test1234567890abcdefghijklmnopqrs"
owner = "test-org"
{}

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"

[runner]
work_dir = "/tmp/fc-runner-test"
"#,
            github_extra
        )
    }

    #[test]
    fn parse_single_repo() {
        let toml = minimal_config(r#"repo = "my-repo""#);
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.github.owner, "test-org");
        assert_eq!(config.github.repo.as_deref(), Some("my-repo"));
        assert_eq!(config.github.all_repos(), vec!["my-repo"]);
    }

    #[test]
    fn parse_multi_repo() {
        let toml = minimal_config(r#"repos = ["repo-a", "repo-b", "repo-c"]"#);
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.github.all_repos(), vec!["repo-a", "repo-b", "repo-c"]);
    }

    #[test]
    fn merge_repo_and_repos() {
        let toml = minimal_config(
            r#"
repo = "repo-x"
repos = ["repo-a", "repo-b"]
"#,
        );
        let config = AppConfig::from_str(&toml).unwrap();
        let repos = config.github.all_repos();
        assert_eq!(repos.len(), 3);
        assert!(repos.contains(&"repo-x".to_string()));
        assert!(repos.contains(&"repo-a".to_string()));
        assert!(repos.contains(&"repo-b".to_string()));
    }

    #[test]
    fn dedup_repo_in_repos() {
        let toml = minimal_config(
            r#"
repo = "repo-a"
repos = ["repo-a", "repo-b"]
"#,
        );
        let config = AppConfig::from_str(&toml).unwrap();
        let repos = config.github.all_repos();
        assert_eq!(repos.len(), 2);
    }

    #[test]
    fn default_labels() {
        let toml = minimal_config(r#"repo = "r""#);
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(
            config.github.labels,
            vec!["self-hosted", "linux", "firecracker"]
        );
    }

    #[test]
    fn custom_labels() {
        let toml = minimal_config(r#"
repo = "r"
labels = ["self-hosted", "arm64"]
"#);
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.github.labels, vec!["self-hosted", "arm64"]);
    }

    #[test]
    fn default_runner_config() {
        let toml = minimal_config(r#"repo = "r""#);
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.runner.poll_interval_secs, 5);
        assert_eq!(config.runner.max_concurrent_jobs, 4);
        assert_eq!(config.runner.vm_timeout_secs, 3600);
        assert_eq!(config.runner.warm_pool_size, 0);
    }

    #[test]
    fn default_firecracker_config() {
        let toml = minimal_config(r#"repo = "r""#);
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.firecracker.vcpu_count, 2);
        assert_eq!(config.firecracker.mem_size_mib, 2048);
        assert_eq!(config.firecracker.secret_injection, "mmds");
        assert!(!config.firecracker.vsock_enabled);
        assert_eq!(config.firecracker.vsock_cid_base, 3);
    }

    #[test]
    fn default_network_config() {
        let toml = minimal_config(r#"repo = "r""#);
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.network.host_ip, "172.16.0.1");
        assert_eq!(config.network.guest_ip, "172.16.0.2");
        assert_eq!(config.network.cidr, "24");
        assert_eq!(config.network.dns, vec!["8.8.8.8", "1.1.1.1"]);
        assert!(config.network.allowed_networks.is_empty());
    }

    #[test]
    fn default_server_config() {
        let toml = minimal_config(r#"repo = "r""#);
        let config = AppConfig::from_str(&toml).unwrap();
        assert!(config.server.enabled);
        assert_eq!(config.server.listen_addr, "0.0.0.0:9090");
        assert!(config.server.api_key.is_none());
    }

    #[test]
    fn parse_organization() {
        let toml = minimal_config(
            r#"
organization = "my-org"
repos = ["repo-a"]
"#,
        );
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.github.organization.as_deref(), Some("my-org"));
    }

    #[test]
    fn parse_pool_config() {
        let toml = format!(
            r#"
{}

[[pool]]
name = "default"
repos = ["repo-a"]
min_ready = 2
max_ready = 4

[[pool]]
name = "heavy"
repos = ["repo-b"]
min_ready = 1
max_ready = 2
vcpu_count = 8
mem_size_mib = 8192
"#,
            minimal_config(r#"repos = ["repo-a", "repo-b"]"#)
        );
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.pool.len(), 2);
        assert_eq!(config.pool[0].name, "default");
        assert_eq!(config.pool[0].min_ready, 2);
        assert_eq!(config.pool[0].max_ready, 4);
        assert!(config.pool[0].vcpu_count.is_none());
        assert_eq!(config.pool[1].name, "heavy");
        assert_eq!(config.pool[1].vcpu_count, Some(8));
        assert_eq!(config.pool[1].mem_size_mib, Some(8192));
    }

    #[test]
    fn parse_vsock_config() {
        let toml = r#"
[github]
token = "ghp_test1234567890abcdefghijklmnopqrs"
owner = "test-org"
repo = "r"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"
vsock_enabled = true
vsock_cid_base = 10

[runner]
work_dir = "/tmp/fc-runner-test"
"#;
        let config = AppConfig::from_str(toml).unwrap();
        assert!(config.firecracker.vsock_enabled);
        assert_eq!(config.firecracker.vsock_cid_base, 10);
    }

    #[test]
    fn parse_server_with_api_key() {
        let toml = format!(
            r#"
{}

[server]
listen_addr = "127.0.0.1:8080"
api_key = "secret-key-123"
"#,
            minimal_config(r#"repo = "r""#)
        );
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.server.listen_addr, "127.0.0.1:8080");
        assert_eq!(config.server.api_key.as_deref(), Some("secret-key-123"));
    }

    #[test]
    fn parse_warm_pool_size() {
        let toml = r#"
[github]
token = "ghp_test1234567890abcdefghijklmnopqrs"
owner = "test-org"
repo = "r"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"

[runner]
work_dir = "/tmp/fc-runner-test"
warm_pool_size = 3
"#;
        let config = AppConfig::from_str(toml).unwrap();
        assert_eq!(config.runner.warm_pool_size, 3);
    }

    #[test]
    fn empty_owner_fails_validate() {
        let toml = r#"
[github]
token = "ghp_test1234567890abcdefghijklmnopqrs"
owner = ""
repo = "r"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"

[runner]
work_dir = "/tmp/fc-runner-test"
"#;
        let config = AppConfig::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("owner"));
    }

    #[test]
    fn no_auth_fails_validate() {
        let toml = r#"
[github]
owner = "test-org"
repo = "r"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"

[runner]
work_dir = "/tmp/fc-runner-test"
"#;
        let config = AppConfig::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("github.token or [github.app]"));
    }

    #[test]
    fn no_repos_fails_validate() {
        let toml = r#"
[github]
token = "ghp_test1234567890abcdefghijklmnopqrs"
owner = "test-org"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"

[runner]
work_dir = "/tmp/fc-runner-test"
"#;
        let config = AppConfig::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("repo"));
    }

    #[test]
    fn zero_vcpu_fails_validate() {
        let toml = r#"
[github]
token = "ghp_test1234567890abcdefghijklmnopqrs"
owner = "test-org"
repo = "r"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"
vcpu_count = 0

[runner]
work_dir = "/tmp/fc-runner-test"
"#;
        let config = AppConfig::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("vcpu_count"));
    }

    #[test]
    fn zero_mem_fails_validate() {
        let toml = r#"
[github]
token = "ghp_test1234567890abcdefghijklmnopqrs"
owner = "test-org"
repo = "r"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"
mem_size_mib = 0

[runner]
work_dir = "/tmp/fc-runner-test"
"#;
        let config = AppConfig::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mem_size_mib"));
    }

    #[test]
    fn zero_max_concurrent_fails_validate() {
        let toml = r#"
[github]
token = "ghp_test1234567890abcdefghijklmnopqrs"
owner = "test-org"
repo = "r"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"

[runner]
work_dir = "/tmp/fc-runner-test"
max_concurrent_jobs = 0
"#;
        let config = AppConfig::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("max_concurrent_jobs"));
    }

    #[test]
    fn github_config_debug_redacts_token() {
        let toml = minimal_config(r#"repo = "r""#);
        let config = AppConfig::from_str(&toml).unwrap();
        let debug = format!("{:?}", config.github);
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("ghp_test"));
    }

    #[test]
    fn pool_defaults() {
        let toml = format!(
            r#"
{}

[[pool]]
name = "test"
repos = ["repo-a"]
"#,
            minimal_config(r#"repos = ["repo-a"]"#)
        );
        let config = AppConfig::from_str(&toml).unwrap();
        assert_eq!(config.pool[0].min_ready, 1);
        assert_eq!(config.pool[0].max_ready, 4);
        assert!(config.pool[0].vcpu_count.is_none());
        assert!(config.pool[0].mem_size_mib.is_none());
    }

    #[test]
    fn invalid_toml_fails() {
        let result = AppConfig::from_str("this is not valid toml {{{}}}");
        assert!(result.is_err());
    }

    #[test]
    fn missing_required_sections_fails() {
        let result = AppConfig::from_str("[github]\nowner = \"test\"");
        assert!(result.is_err());
    }

    #[test]
    fn jailer_requires_uid_gid() {
        let toml = r#"
[github]
token = "ghp_test1234567890abcdefghijklmnopqrs"
owner = "test-org"
repo = "r"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/rootfs.ext4"
jailer_path = "/usr/local/bin/jailer"

[runner]
work_dir = "/tmp/fc-runner-test"
"#;
        let config = AppConfig::from_str(toml).unwrap();
        let result = config.validate();
        // Should fail because jailer_uid is missing (if jailer binary exists)
        // or fail because jailer_path doesn't exist
        assert!(result.is_err());
    }
}
