use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde::Serialize;
use tokio::sync::{Mutex, mpsc};
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::config::{AppConfig, PoolConfig};
use crate::firecracker::MicroVm;
use crate::github::GitHubClient;

/// Runtime status of a pool, returned by the management API.
#[derive(Clone, Serialize)]
pub struct PoolStatus {
    pub name: String,
    pub repos: Vec<String>,
    pub min_ready: usize,
    pub max_ready: usize,
    pub active: usize,
    pub idle_slots: usize,
    pub paused: bool,
}

/// Manages a named pool of warm VMs for a set of repos.
pub struct PoolManager {
    pub pool_config: PoolConfig,
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    cancel: CancellationToken,
    slot_pool: Arc<Mutex<Vec<usize>>>,
    active_count: Arc<AtomicUsize>,
    paused: Arc<AtomicBool>,
    /// Runtime-adjustable min_ready
    min_ready: Arc<AtomicUsize>,
    /// Runtime-adjustable max_ready
    max_ready: Arc<AtomicUsize>,
}

impl PoolManager {
    pub fn new(
        pool_config: PoolConfig,
        config: Arc<AppConfig>,
        github: Arc<GitHubClient>,
        cancel: CancellationToken,
        slots: Vec<usize>,
    ) -> Self {
        let min_ready = pool_config.min_ready;
        let max_ready = pool_config.max_ready;
        Self {
            pool_config,
            config,
            github,
            cancel,
            slot_pool: Arc::new(Mutex::new(slots)),
            active_count: Arc::new(AtomicUsize::new(0)),
            paused: Arc::new(AtomicBool::new(false)),
            min_ready: Arc::new(AtomicUsize::new(min_ready)),
            max_ready: Arc::new(AtomicUsize::new(max_ready)),
        }
    }

    // ── Runtime management methods ──────────────────────────────────

    /// Pause the pool — stop creating new VMs.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        tracing::info!(pool = %self.pool_config.name, "pool paused");
    }

    /// Resume a paused pool.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        tracing::info!(pool = %self.pool_config.name, "pool resumed");
    }

    /// Check if the pool is paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Scale the pool by adjusting min_ready and/or max_ready at runtime.
    pub fn scale(&self, new_min: Option<usize>, new_max: Option<usize>) {
        if let Some(min) = new_min {
            self.min_ready.store(min, Ordering::Relaxed);
        }
        if let Some(max) = new_max {
            self.max_ready.store(max, Ordering::Relaxed);
        }
        tracing::info!(
            pool = %self.pool_config.name,
            min_ready = self.min_ready.load(Ordering::Relaxed),
            max_ready = self.max_ready.load(Ordering::Relaxed),
            "pool scaled"
        );
    }

    /// Get the current pool status.
    pub async fn status(&self) -> PoolStatus {
        PoolStatus {
            name: self.pool_config.name.clone(),
            repos: self.pool_config.repos.clone(),
            min_ready: self.min_ready.load(Ordering::Relaxed),
            max_ready: self.max_ready.load(Ordering::Relaxed),
            active: self.active_count.load(Ordering::Relaxed),
            idle_slots: self.slot_pool.lock().await.len(),
            paused: self.is_paused(),
        }
    }

    // ── Pool lifecycle ──────────────────────────────────────────────

    /// Run the pool, maintaining min_ready idle VMs at all times.
    pub async fn run(&self) -> anyhow::Result<()> {
        let pool_name = &self.pool_config.name;
        let min_ready = self.min_ready.load(Ordering::Relaxed);
        let repos = &self.pool_config.repos;

        if repos.is_empty() {
            anyhow::bail!("pool '{}' has no repos configured", pool_name);
        }

        tracing::info!(
            pool = %pool_name,
            min_ready,
            max_ready = self.max_ready.load(Ordering::Relaxed),
            repos = ?repos,
            "starting pool manager"
        );

        let (done_tx, mut done_rx) =
            mpsc::channel::<(usize, String)>(self.max_ready.load(Ordering::Relaxed) * 2);

        // Spawn initial pool — distribute across repos round-robin
        for i in 0..min_ready {
            let repo = repos[i % repos.len()].clone();
            let slot = {
                let mut pool = self.slot_pool.lock().await;
                match pool.pop() {
                    Some(s) => s,
                    None => {
                        tracing::warn!(pool = %pool_name, "no slots available for initial pool spawn");
                        break;
                    }
                }
            };
            self.spawn_vm(slot, repo, done_tx.clone());
        }

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    tracing::info!(pool = %pool_name, "shutdown signal, waiting for pool VMs...");
                    self.wait_for_active(Duration::from_secs(300)).await;
                    break;
                }
                Some((slot, repo)) = done_rx.recv() => {
                    // Return slot to pool
                    self.slot_pool.lock().await.push(slot);

                    tokio::time::sleep(Duration::from_secs(3)).await;

                    if self.cancel.is_cancelled() {
                        break;
                    }

                    // Skip spawning replacements if paused
                    if self.is_paused() {
                        tracing::debug!(pool = %pool_name, "pool is paused, not spawning replacement");
                        continue;
                    }

                    // Check if we need a replacement
                    let active = self.active_count.load(Ordering::Relaxed);
                    let max_ready = self.max_ready.load(Ordering::Relaxed);
                    if active >= max_ready {
                        tracing::debug!(pool = %pool_name, active, "at max_ready, not spawning replacement");
                        continue;
                    }

                    let new_slot = {
                        let mut pool = self.slot_pool.lock().await;
                        match pool.pop() {
                            Some(s) => s,
                            None => {
                                tracing::warn!(pool = %pool_name, "no slots available for replacement");
                                continue;
                            }
                        }
                    };

                    tracing::info!(
                        pool = %pool_name,
                        slot = new_slot,
                        repo = %repo,
                        "spawning pool replacement"
                    );
                    self.spawn_vm(new_slot, repo, done_tx.clone());
                }
            }
        }
        Ok(())
    }

    fn spawn_vm(&self, slot: usize, repo: String, done_tx: mpsc::Sender<(usize, String)>) {
        let config = self.config.clone();
        let github = self.github.clone();
        let active_count = self.active_count.clone();
        let pool_name = self.pool_config.name.clone();
        let vcpu_override = self.pool_config.vcpu_count;
        let mem_override = self.pool_config.mem_size_mib;
        let cancel = self.cancel.clone();

        tokio::spawn(async move {
            active_count.fetch_add(1, Ordering::Relaxed);
            tracing::info!(pool = %pool_name, slot, repo = %repo, "starting pool VM");

            let result = run_pool_vm(
                config,
                github.clone(),
                slot,
                &repo,
                vcpu_override,
                mem_override,
                cancel,
            )
            .await;

            github.remove_offline_runners(&repo).await;

            match &result {
                Ok(()) => {
                    tracing::info!(pool = %pool_name, slot, repo = %repo, "pool VM completed successfully");
                }
                Err(e) => {
                    tracing::error!(pool = %pool_name, slot, repo = %repo, error = %e, "pool VM failed");
                }
            }

            active_count.fetch_sub(1, Ordering::Relaxed);
            let _ = done_tx.send((slot, repo)).await;
        });
    }

    async fn wait_for_active(&self, max_wait: Duration) {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let count = self.active_count.load(Ordering::Relaxed);
            if count == 0 {
                tracing::info!(pool = %self.pool_config.name, "all pool VMs completed");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(pool = %self.pool_config.name, remaining = count, "shutdown timeout");
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    #[allow(dead_code)]
    pub fn get_active_count(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub async fn available_slots(&self) -> usize {
        self.slot_pool.lock().await.len()
    }
}

async fn run_pool_vm(
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    slot: usize,
    repo: &str,
    vcpu_override: Option<u32>,
    mem_override: Option<u32>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let is_org = github.is_org_mode();

    let (reg_token, registration_url) = if is_org {
        let org = config.github.organization.as_deref().unwrap_or("unknown");
        tracing::info!(
            slot,
            organization = org,
            "requesting org registration token for pool VM"
        );
        let token = github.generate_org_registration_token().await?;
        let url = format!("https://github.com/{}", org);
        (token, url)
    } else {
        tracing::info!(slot, repo = %repo, "requesting registration token for pool VM");
        let token = github.generate_registration_token(repo).await?;
        let url = format!("https://github.com/{}/{}", config.github.owner, repo);
        (token, url)
    };
    let runner_name = format!(
        "fc-pool-{}-{}",
        slot,
        &uuid::Uuid::new_v4().to_string()[..8]
    );

    // Apply per-pool resource overrides
    let mut fc_config = config.firecracker.clone();
    if let Some(vcpu) = vcpu_override {
        fc_config.vcpu_count = vcpu;
    }
    if let Some(mem) = mem_override {
        fc_config.mem_size_mib = mem;
    }

    let mut vm = MicroVm::new(
        0, // no specific job_id for pool VMs
        &fc_config,
        &config.network,
        &config.runner.work_dir,
        config.runner.vm_timeout_secs,
        slot,
        cancel,
    );
    if config.cache_service.enabled {
        vm.cache_service_token = config.cache_service.token.clone();
        vm.cache_service_port = config
            .server
            .listen_addr
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok());
    }
    let ephemeral = config.runner.ephemeral;
    let mut env_content = format!(
        "RUNNER_MODE=register\nRUNNER_TOKEN={}\nREPO_URL={}\nRUNNER_NAME={}\nVM_ID={}\nHOSTNAME={}\nSHUTDOWN_ON_EXIT=true\nEPHEMERAL={}\n",
        reg_token, registration_url, runner_name, vm.vm_id, vm.vm_id, ephemeral
    );
    if config.cache_service.enabled
        && let (Some(token), Some(port)) = (
            &config.cache_service.token,
            config
                .server
                .listen_addr
                .rsplit(':')
                .next()
                .and_then(|p| p.parse::<u16>().ok()),
        )
    {
        let cache_url = format!("http://{}:{}/", vm.host_ip, port);
        env_content.push_str(&format!("CACHE_URL={}\nCACHE_TOKEN={}\n", cache_url, token));
    }
    vm.execute(&env_content).await
}
