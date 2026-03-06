use anyhow::{bail, Context};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub github: GitHubConfig,
    pub firecracker: FirecrackerConfig,
    pub runner: RunnerConfig,
}

#[derive(Clone, Deserialize)]
pub struct GitHubConfig {
    pub token: String,
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

impl AppConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading config: {}", path))?;
        let config: AppConfig =
            toml::from_str(&content).with_context(|| "parsing config TOML")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.github.token.is_empty() {
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

        let required_paths = [
            ("firecracker.kernel_path", &self.firecracker.kernel_path),
            ("firecracker.rootfs_golden", &self.firecracker.rootfs_golden),
            ("firecracker.binary_path", &self.firecracker.binary_path),
            (
                "firecracker.vm_config_template",
                &self.firecracker.vm_config_template,
            ),
        ];
        for (name, path) in required_paths {
            if !Path::new(path).exists() {
                bail!("{} does not exist: {}", name, path);
            }
        }

        let work_dir = Path::new(&self.runner.work_dir);
        if !work_dir.exists() {
            std::fs::create_dir_all(work_dir)
                .with_context(|| format!("creating work_dir: {}", self.runner.work_dir))?;
        }

        Ok(())
    }
}
