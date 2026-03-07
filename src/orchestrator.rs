use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::firecracker::MicroVm;
use crate::github::GitHubClient;
use crate::metrics;
use crate::pool::PoolManager;

pub struct Orchestrator {
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    seen_jobs: Arc<Mutex<HashSet<u64>>>,
    cancel: CancellationToken,
    semaphore: Arc<Semaphore>,
    active_jobs: Arc<Mutex<usize>>,
    /// Pool of slot indices for per-VM TAP device allocation.
    slot_pool: Arc<Mutex<Vec<usize>>>,
}

impl Orchestrator {
    pub fn new(config: Arc<AppConfig>, cancel: CancellationToken) -> anyhow::Result<Self> {
        let github = Arc::new(GitHubClient::new(config.github.clone())?);
        let max_jobs = config.runner.max_concurrent_jobs;
        let repos = github.repos();
        tracing::info!(
            max_concurrent_jobs = max_jobs,
            warm_pool_size = config.runner.warm_pool_size,
            owner = %github.owner(),
            repos = ?repos,
            "orchestrator configured for {} repo(s)",
            repos.len()
        );
        Ok(Self {
            config,
            github,
            seen_jobs: Arc::new(Mutex::new(HashSet::new())),
            cancel,
            semaphore: Arc::new(Semaphore::new(max_jobs)),
            active_jobs: Arc::new(Mutex::new(0)),
            slot_pool: Arc::new(Mutex::new((0..max_jobs).collect())),
        })
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        if !self.config.pool.is_empty() {
            self.run_pools().await
        } else if self.config.runner.warm_pool_size > 0 {
            self.run_warm_pool().await
        } else {
            self.run_reactive().await
        }
    }

    // ── Pool mode (named pools with per-pool config) ───────────────────

    async fn run_pools(&self) -> anyhow::Result<()> {
        let pools = &self.config.pool;
        tracing::info!(
            pool_count = pools.len(),
            "orchestrator starting (pool mode)"
        );

        // Distribute slots across pools proportionally
        let total_slots: usize = pools.iter().map(|p| p.max_ready).sum();
        if total_slots > self.config.runner.max_concurrent_jobs {
            tracing::warn!(
                total_pool_slots = total_slots,
                max_concurrent_jobs = self.config.runner.max_concurrent_jobs,
                "total pool max_ready exceeds max_concurrent_jobs, some pools may not get all requested slots"
            );
        }

        let mut all_slots: Vec<usize> = (0..self.config.runner.max_concurrent_jobs).collect();
        let mut pool_managers = Vec::new();

        for pool_config in pools {
            let slots_for_pool: Vec<usize> = (0..pool_config.max_ready)
                .filter_map(|_| all_slots.pop())
                .collect();

            if slots_for_pool.is_empty() {
                tracing::warn!(pool = %pool_config.name, "no slots available for pool");
                continue;
            }

            tracing::info!(
                pool = %pool_config.name,
                slots = slots_for_pool.len(),
                repos = ?pool_config.repos,
                "allocating slots to pool"
            );

            let manager = PoolManager::new(
                pool_config.clone(),
                self.config.clone(),
                self.github.clone(),
                self.cancel.clone(),
                slots_for_pool,
            );
            pool_managers.push(manager);
        }

        // Run all pool managers concurrently
        let mut handles = Vec::new();
        for manager in pool_managers {
            let handle = tokio::spawn(async move {
                if let Err(e) = manager.run().await {
                    tracing::error!(pool = %manager.pool_config.name, error = %e, "pool manager failed");
                }
            });
            handles.push(handle);
        }

        // Wait for all pool managers to finish
        for handle in handles {
            let _ = handle.await;
        }

        Ok(())
    }

    // ── Reactive mode (JIT) ──────────────────────────────────────────

    async fn run_reactive(&self) -> anyhow::Result<()> {
        let mut ticker = interval(Duration::from_secs(self.config.runner.poll_interval_secs));
        tracing::info!(
            poll_interval = self.config.runner.poll_interval_secs,
            "orchestrator starting (reactive/JIT mode)"
        );

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    tracing::info!("shutdown signal received, waiting for in-flight jobs...");
                    self.wait_for_active_jobs(Duration::from_secs(300)).await;
                    break;
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.poll_once().await {
                        tracing::error!(error = %e, "poll cycle failed");
                    }
                }
            }
        }
        Ok(())
    }

    async fn poll_once(&self) -> anyhow::Result<()> {
        for repo in self.github.repos() {
            if let Err(e) = self.poll_repo(&repo).await {
                metrics::POLL_CYCLES.with_label_values(&["error"]).inc();
                tracing::error!(
                    repo = %repo,
                    error = %e,
                    "failed to poll repo, skipping"
                );
            }
        }
        metrics::POLL_CYCLES.with_label_values(&["ok"]).inc();
        Ok(())
    }

    async fn poll_repo(&self, repo: &str) -> anyhow::Result<()> {
        let runs = self.github.list_queued_runs(repo).await?;
        tracing::debug!(repo = %repo, count = runs.len(), "found queued runs");

        for run in runs {
            let jobs = self.github.list_queued_jobs(repo, run.id).await?;
            for job in jobs {
                if !self.labels_match(&job.labels) {
                    continue;
                }

                {
                    let mut seen = self.seen_jobs.lock().await;
                    if !seen.insert(job.id) {
                        tracing::trace!(job_id = job.id, "already dispatched, skipping");
                        continue;
                    }
                }

                tracing::info!(
                    job_id = job.id,
                    run_id = job.run_id,
                    repo = %repo,
                    "dispatching new job"
                );
                metrics::JOBS_DISPATCHED.with_label_values(&[repo]).inc();
                self.dispatch_jit_job(job.id, repo.to_string());
            }
        }
        Ok(())
    }

    fn labels_match(&self, job_labels: &[String]) -> bool {
        self.config
            .github
            .labels
            .iter()
            .all(|l| job_labels.contains(l))
    }

    fn dispatch_jit_job(&self, job_id: u64, repo: String) {
        let config = self.config.clone();
        let github = self.github.clone();
        let seen_jobs = self.seen_jobs.clone();
        let semaphore = self.semaphore.clone();
        let active_jobs = self.active_jobs.clone();
        let slot_pool = self.slot_pool.clone();

        tokio::spawn(async move {
            let _permit = match semaphore.acquire().await {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::error!(job_id, "semaphore closed, cannot dispatch job");
                    return;
                }
            };

            let slot = {
                let mut pool = slot_pool.lock().await;
                pool.pop().expect("semaphore guarantees a slot is available")
            };

            *active_jobs.lock().await += 1;
            metrics::JOBS_ACTIVE.inc();
            metrics::POOL_SLOTS_AVAILABLE.dec();
            tracing::info!(job_id, repo = %repo, slot, "job started (permit acquired, slot assigned)");

            let timer = metrics::VM_BOOT_DURATION.with_label_values(&[&repo]).start_timer();
            let result = run_jit_job(config.clone(), github.clone(), job_id, &repo, slot).await;
            timer.observe_duration();

            slot_pool.lock().await.push(slot);
            *active_jobs.lock().await -= 1;
            metrics::JOBS_ACTIVE.dec();
            metrics::POOL_SLOTS_AVAILABLE.inc();

            // Clean up offline runners left by this (and any previous) VMs
            github.remove_offline_runners(&repo).await;

            match result {
                Ok(()) => {
                    seen_jobs.lock().await.remove(&job_id);
                    metrics::JOBS_COMPLETED.with_label_values(&[&repo]).inc();
                    tracing::info!(job_id, repo = %repo, slot, "job completed successfully");
                }
                Err(e) => {
                    metrics::JOBS_FAILED.with_label_values(&[&repo]).inc();
                    tracing::error!(job_id, repo = %repo, slot, error = %e, "job failed (will not retry)");
                }
            }
        });
    }

    // ── Warm pool mode (registration tokens) ─────────────────────────

    async fn run_warm_pool(&self) -> anyhow::Result<()> {
        let pool_size = self.config.runner.warm_pool_size;
        let repos = self.github.repos();
        if repos.is_empty() {
            anyhow::bail!("no repos configured for warm pool");
        }

        tracing::info!(
            pool_size,
            repos = ?repos,
            "orchestrator starting (warm pool mode)"
        );

        // Channel for VMs to signal slot return
        let (done_tx, mut done_rx) = mpsc::channel::<(usize, String)>(pool_size * 2);

        // Spawn initial pool — distribute across repos round-robin
        for i in 0..pool_size {
            let repo = repos[i % repos.len()].clone();
            let slot = {
                let mut pool = self.slot_pool.lock().await;
                pool.pop().expect("pool should have enough slots")
            };
            self.spawn_warm_vm(slot, repo, done_tx.clone());
        }

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    tracing::info!("shutdown signal, waiting for warm pool VMs...");
                    self.wait_for_active_jobs(Duration::from_secs(300)).await;
                    break;
                }
                Some((slot, repo)) = done_rx.recv() => {
                    // Return slot to pool
                    self.slot_pool.lock().await.push(slot);

                    // Brief delay before spawning replacement
                    tokio::time::sleep(Duration::from_secs(3)).await;

                    if self.cancel.is_cancelled() {
                        break;
                    }

                    // Get a new slot for the replacement
                    let new_slot = {
                        let mut pool = self.slot_pool.lock().await;
                        match pool.pop() {
                            Some(s) => s,
                            None => {
                                tracing::warn!("no slots available for warm pool replacement");
                                continue;
                            }
                        }
                    };

                    tracing::info!(slot = new_slot, repo = %repo, "spawning warm pool replacement");
                    self.spawn_warm_vm(new_slot, repo, done_tx.clone());
                }
            }
        }
        Ok(())
    }

    fn spawn_warm_vm(&self, slot: usize, repo: String, done_tx: mpsc::Sender<(usize, String)>) {
        let config = self.config.clone();
        let github = self.github.clone();
        let active_jobs = self.active_jobs.clone();

        tokio::spawn(async move {
            *active_jobs.lock().await += 1;
            metrics::JOBS_ACTIVE.inc();
            metrics::POOL_SLOTS_AVAILABLE.dec();
            tracing::info!(slot, repo = %repo, "starting warm pool VM");

            let timer = metrics::VM_BOOT_DURATION.with_label_values(&[&repo]).start_timer();
            let result = run_warm_vm(config, github.clone(), slot, &repo).await;
            timer.observe_duration();

            // Clean up offline runners left by this VM
            github.remove_offline_runners(&repo).await;

            match &result {
                Ok(()) => {
                    metrics::JOBS_COMPLETED.with_label_values(&[&repo]).inc();
                    tracing::info!(slot, repo = %repo, "warm pool VM completed job successfully");
                }
                Err(e) => {
                    metrics::JOBS_FAILED.with_label_values(&[&repo]).inc();
                    tracing::error!(slot, repo = %repo, error = %e, "warm pool VM failed");
                }
            }

            *active_jobs.lock().await -= 1;
            metrics::JOBS_ACTIVE.dec();
            metrics::POOL_SLOTS_AVAILABLE.inc();

            // Signal that this slot is done and needs replacement
            let _ = done_tx.send((slot, repo)).await;
        });
    }

    /// Wait for all active jobs to finish, with a timeout.
    async fn wait_for_active_jobs(&self, max_wait: Duration) {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let count = *self.active_jobs.lock().await;
            if count == 0 {
                tracing::info!("all in-flight jobs completed");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(remaining = count, "shutdown timeout, some jobs still running");
                break;
            }
            tracing::info!(remaining = count, "waiting for in-flight jobs...");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

// ── Job runners ──────────────────────────────────────────────────────

async fn run_jit_job(
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    job_id: u64,
    repo: &str,
    slot: usize,
) -> anyhow::Result<()> {
    tracing::info!(job_id, repo = %repo, slot, "requesting JIT token");
    let jit_token = github.generate_jit_config(repo, job_id).await?;
    tracing::info!(job_id, repo = %repo, "JIT token acquired");

    let repo_url = format!(
        "https://github.com/{}/{}",
        config.github.owner, repo
    );
    let vm = MicroVm::new(
        job_id,
        &config.firecracker,
        &config.network,
        &config.runner.work_dir,
        config.runner.vm_timeout_secs,
        slot,
    );
    let env_content = format!(
        "RUNNER_MODE=jit\nRUNNER_TOKEN={}\nREPO_URL={}\nVM_ID={}\n",
        jit_token, repo_url, vm.vm_id
    );
    vm.execute(&env_content).await
}

async fn run_warm_vm(
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    slot: usize,
    repo: &str,
) -> anyhow::Result<()> {
    tracing::info!(slot, repo = %repo, "requesting registration token");
    let reg_token = github.generate_registration_token(repo).await?;
    tracing::info!(slot, repo = %repo, "registration token acquired");

    let repo_url = format!(
        "https://github.com/{}/{}",
        config.github.owner, repo
    );
    let runner_name = format!(
        "fc-warm-{}-{}",
        slot,
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let vm = MicroVm::new(
        0, // no specific job_id for warm pool VMs
        &config.firecracker,
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
