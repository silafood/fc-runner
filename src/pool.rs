use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::config::{AppConfig, PoolConfig};
use crate::firecracker::MicroVm;
use crate::github::GitHubClient;

/// Manages a named pool of warm VMs for a set of repos.
pub struct PoolManager {
    pub pool_config: PoolConfig,
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    cancel: CancellationToken,
    slot_pool: Arc<Mutex<Vec<usize>>>,
    active_count: Arc<Mutex<usize>>,
}

impl PoolManager {
    pub fn new(
        pool_config: PoolConfig,
        config: Arc<AppConfig>,
        github: Arc<GitHubClient>,
        cancel: CancellationToken,
        slots: Vec<usize>,
    ) -> Self {
        Self {
            pool_config,
            config,
            github,
            cancel,
            slot_pool: Arc::new(Mutex::new(slots)),
            active_count: Arc::new(Mutex::new(0)),
        }
    }

    /// Run the pool, maintaining min_ready idle VMs at all times.
    pub async fn run(&self) -> anyhow::Result<()> {
        let pool_name = &self.pool_config.name;
        let min_ready = self.pool_config.min_ready;
        let repos = &self.pool_config.repos;

        if repos.is_empty() {
            anyhow::bail!("pool '{}' has no repos configured", pool_name);
        }

        tracing::info!(
            pool = %pool_name,
            min_ready,
            max_ready = self.pool_config.max_ready,
            repos = ?repos,
            "starting pool manager"
        );

        let (done_tx, mut done_rx) = mpsc::channel::<(usize, String)>(self.pool_config.max_ready * 2);

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

                    // Check if we need a replacement (stay at min_ready)
                    let active = *self.active_count.lock().await;
                    if active >= self.pool_config.max_ready {
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

        tokio::spawn(async move {
            *active_count.lock().await += 1;
            tracing::info!(pool = %pool_name, slot, repo = %repo, "starting pool VM");

            let result = run_pool_vm(
                config,
                github.clone(),
                slot,
                &repo,
                vcpu_override,
                mem_override,
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

            *active_count.lock().await -= 1;
            let _ = done_tx.send((slot, repo)).await;
        });
    }

    async fn wait_for_active(&self, max_wait: Duration) {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let count = *self.active_count.lock().await;
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

    pub async fn active_count(&self) -> usize {
        *self.active_count.lock().await
    }

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
) -> anyhow::Result<()> {
    tracing::info!(slot, repo = %repo, "requesting registration token for pool VM");
    let reg_token = github.generate_registration_token(repo).await?;

    let repo_url = format!(
        "https://github.com/{}/{}",
        config.github.owner, repo
    );
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

    let vm = MicroVm::new(
        0, // no specific job_id for pool VMs
        &fc_config,
        &config.network,
        &config.runner.work_dir,
        config.runner.vm_timeout_secs,
        slot,
    );
    let env_content = format!(
        "RUNNER_MODE=register\nRUNNER_TOKEN={}\nREPO_URL={}\nRUNNER_NAME={}\nVM_ID={}\n",
        reg_token, repo_url, runner_name, vm.vm_id
    );
    vm.execute(&env_content).await
}
