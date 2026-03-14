use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::time::{Duration, interval};
use tokio_util::sync::CancellationToken;

use crate::api::ServerState;
use crate::config::AppConfig;
use crate::github::GitHubClient;
use crate::metrics;
use crate::scheduler::PoolManager;
use crate::vm::{MicroVm, VmRunContext};

pub struct Orchestrator {
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    seen_jobs: Arc<Mutex<HashSet<u64>>>,
    cancel: CancellationToken,
    semaphore: Arc<Semaphore>,
    active_jobs: Arc<Mutex<usize>>,
    /// Pool of slot indices for per-VM TAP device allocation.
    slot_pool: Arc<Mutex<Vec<usize>>>,
    server_state: Arc<ServerState>,
}

impl Orchestrator {
    pub fn new(
        config: Arc<AppConfig>,
        cancel: CancellationToken,
        server_state: Arc<ServerState>,
    ) -> anyhow::Result<Self> {
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
            server_state,
        })
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        // Clean up stale offline runners from previous fc-runner sessions
        self.cleanup_stale_runners().await;

        if !self.config.pool.is_empty() {
            self.run_pools().await
        } else if self.config.runner.warm_pool_size > 0 {
            self.run_warm_pool().await
        } else {
            self.run_reactive().await
        }
    }

    /// Remove any offline runners with the `fc-` prefix left over from previous sessions.
    async fn cleanup_stale_runners(&self) {
        tracing::info!("cleaning up stale offline runners from previous sessions...");
        if self.github.is_org_mode() {
            self.github.remove_org_offline_runners().await;
        } else {
            for repo in self.github.repos() {
                self.github.remove_offline_runners(&repo).await;
            }
        }
    }

    // ── Pool mode (named pools with per-pool config) ───────────────────

    async fn run_pools(&self) -> anyhow::Result<()> {
        let pools = &self.config.pool;
        tracing::info!(
            pool_count = pools.len(),
            "orchestrator starting (pool mode)"
        );

        self.server_state.set_mode("pools").await;

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
        let mut pool_managers: std::collections::HashMap<String, Arc<PoolManager>> =
            std::collections::HashMap::new();

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

            let manager = Arc::new(PoolManager::new(
                pool_config.clone(),
                self.config.clone(),
                self.github.clone(),
                self.cancel.clone(),
                slots_for_pool,
                self.server_state.log_tx.clone(),
            ));
            pool_managers.insert(pool_config.name.clone(), manager);
        }

        // Register pool managers with server state for API access
        self.server_state.set_pools(pool_managers.clone()).await;

        // Run all pool managers concurrently
        let mut handles = Vec::new();
        for (name, manager) in &pool_managers {
            let name = name.clone();
            let manager = manager.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) = manager.run().await {
                    tracing::error!(pool = %name, error = ?e, "pool manager failed");
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
        self.server_state.set_mode("jit").await;
        let mut ticker = interval(Duration::from_secs(self.config.runner.poll_interval_secs));
        tracing::info!(
            poll_interval = self.config.runner.poll_interval_secs,
            "orchestrator starting (reactive/JIT mode)"
        );

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    tracing::info!("shutdown signal received, waiting for in-flight jobs...");
                    self.wait_for_active_jobs(Duration::from_secs(10)).await;
                    break;
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.poll_once().await {
                        tracing::error!(error = ?e, "poll cycle failed");
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
                    error = ?e,
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
        let server_state = self.server_state.clone();
        let cancel = self.cancel.clone();

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
                pool.pop()
                    .expect("semaphore guarantees a slot is available")
            };

            *active_jobs.lock().await += 1;
            metrics::JOBS_ACTIVE.inc();
            metrics::POOL_SLOTS_AVAILABLE.dec();
            tracing::info!(job_id, repo = %repo, slot, "job started (permit acquired, slot assigned)");

            let vm_id = format!("fc-{}-slot{}", job_id, slot);
            server_state
                .register_vm(crate::api::VmInfo {
                    vm_id: vm_id.clone(),
                    job_id,
                    repo: repo.clone(),
                    slot,
                    started_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        .to_string(),
                })
                .await;

            let ctx = VmRunContext::new(config.clone(), github.clone(), slot, cancel)
                .log_tx(server_state.log_tx.clone());
            let timer = metrics::VM_BOOT_DURATION
                .with_label_values(&[&repo])
                .start_timer();
            let result = run_jit_job(ctx, job_id, &repo).await;
            timer.observe_duration();

            server_state.unregister_vm(&vm_id).await;
            slot_pool.lock().await.push(slot);
            *active_jobs.lock().await -= 1;
            metrics::JOBS_ACTIVE.dec();
            metrics::POOL_SLOTS_AVAILABLE.inc();

            // Clean up offline runners left by this (and any previous) VMs
            if github.is_org_mode() {
                github.remove_org_offline_runners().await;
            } else {
                github.remove_offline_runners(&repo).await;
            }

            // Always remove from seen_jobs so the job ID doesn't block retries
            seen_jobs.lock().await.remove(&job_id);

            match result {
                Ok(()) => {
                    metrics::JOBS_COMPLETED.with_label_values(&[&repo]).inc();
                    tracing::info!(job_id, repo = %repo, slot, "job completed successfully");
                }
                Err(e) => {
                    metrics::JOBS_FAILED.with_label_values(&[&repo]).inc();
                    tracing::error!(job_id, repo = %repo, slot, error = ?e, "job failed");
                }
            }
        });
    }

    // ── Warm pool mode (registration tokens) ─────────────────────────

    async fn run_warm_pool(&self) -> anyhow::Result<()> {
        self.server_state.set_mode("warm_pool").await;
        let pool_size = self.config.runner.warm_pool_size;
        let ephemeral = self.config.runner.ephemeral;
        let max_vms = self.config.runner.max_concurrent_jobs;
        let repos = self.github.repos();
        let is_org = self.github.is_org_mode();
        if repos.is_empty() && !is_org {
            anyhow::bail!(
                "no repos configured for warm pool (set github.repo/repos or github.organization)"
            );
        }

        if is_org {
            let org = self
                .config
                .github
                .organization
                .as_deref()
                .unwrap_or("unknown");
            tracing::info!(
                pool_size,
                ephemeral,
                organization = org,
                "orchestrator starting (warm pool mode, org-level runners)"
            );
        } else {
            tracing::info!(
                pool_size,
                ephemeral,
                repos = ?repos,
                "orchestrator starting (warm pool mode)"
            );
        }

        // Channel for VMs to signal slot return (crash recovery + ephemeral replacement)
        let (done_tx, mut done_rx) = mpsc::channel::<(usize, String)>(max_vms * 2);

        // Spawn initial pool
        for i in 0..pool_size {
            let repo = if is_org {
                String::new()
            } else {
                repos[i % repos.len()].clone()
            };
            let slot = {
                let mut pool = self.slot_pool.lock().await;
                pool.pop().expect("pool should have enough slots")
            };
            self.spawn_warm_vm(slot, repo, done_tx.clone());
        }

        if ephemeral {
            // Ephemeral mode: VMs exit after one job, spawn replacements
            self.run_warm_pool_ephemeral(done_tx, &mut done_rx).await
        } else {
            // Non-ephemeral mode: VMs stay alive, auto-scale standbys when runners are busy
            self.run_warm_pool_autoscale(done_tx, &mut done_rx).await
        }
    }

    /// Ephemeral warm pool: wait for VMs to exit and spawn replacements.
    async fn run_warm_pool_ephemeral(
        &self,
        done_tx: mpsc::Sender<(usize, String)>,
        done_rx: &mut mpsc::Receiver<(usize, String)>,
    ) -> anyhow::Result<()> {
        tracing::info!("ephemeral warm pool replacement loop started");
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    tracing::info!("shutdown signal, waiting for warm pool VMs...");
                    self.wait_for_active_jobs(Duration::from_secs(10)).await;
                    break;
                }
                Some((slot, repo)) = done_rx.recv() => {
                    tracing::info!(
                        slot,
                        repo = %repo,
                        "received VM completion signal, scheduling replacement"
                    );
                    self.slot_pool.lock().await.push(slot);
                    tracing::debug!(slot, "slot returned to pool, waiting 3s before replacement");
                    tokio::time::sleep(Duration::from_secs(3)).await;

                    if self.cancel.is_cancelled() {
                        tracing::info!("shutdown cancelled during replacement delay");
                        break;
                    }

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

                    tracing::info!(
                        old_slot = slot,
                        new_slot,
                        repo = %repo,
                        "spawning warm pool replacement VM"
                    );
                    self.spawn_warm_vm(new_slot, repo, done_tx.clone());
                }
            }
        }
        Ok(())
    }

    /// Non-ephemeral warm pool: runners stay alive across jobs.
    /// Periodically polls GitHub API to count idle vs busy runners.
    /// Spawns standby VMs when idle runners drop below warm_pool_size
    /// and total VMs are under max_concurrent_jobs.
    async fn run_warm_pool_autoscale(
        &self,
        done_tx: mpsc::Sender<(usize, String)>,
        done_rx: &mut mpsc::Receiver<(usize, String)>,
    ) -> anyhow::Result<()> {
        let pool_size = self.config.runner.warm_pool_size;
        let max_vms = self.config.runner.max_concurrent_jobs;
        let repos = self.github.repos();
        let is_org = self.github.is_org_mode();
        let check_interval = Duration::from_secs(self.config.runner.poll_interval_secs);

        let mut ticker = interval(check_interval);

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    tracing::info!("shutdown signal, waiting for warm pool VMs...");
                    self.wait_for_active_jobs(Duration::from_secs(10)).await;
                    break;
                }
                // Handle crashed/exited VMs — spawn replacement
                Some((slot, repo)) = done_rx.recv() => {
                    self.slot_pool.lock().await.push(slot);
                    tracing::info!(slot, repo = %repo, "non-ephemeral VM exited, scheduling replacement");

                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if self.cancel.is_cancelled() { break; }

                    let new_slot = {
                        let mut pool = self.slot_pool.lock().await;
                        match pool.pop() {
                            Some(s) => s,
                            None => {
                                tracing::warn!("no slots available for replacement");
                                continue;
                            }
                        }
                    };
                    let repo = if is_org { String::new() } else { repo };
                    self.spawn_warm_vm(new_slot, repo, done_tx.clone());
                }
                // Periodically check runner status and scale up if needed
                _ = ticker.tick() => {
                    if let Err(e) = self.autoscale_check(pool_size, max_vms, &repos, is_org, &done_tx).await {
                        tracing::debug!(error = %e, "autoscale check failed");
                    }
                }
            }
        }
        Ok(())
    }

    /// Check GitHub API for runner status and spawn standbys if idle < warm_pool_size.
    async fn autoscale_check(
        &self,
        target_idle: usize,
        max_vms: usize,
        repos: &[String],
        is_org: bool,
        done_tx: &mpsc::Sender<(usize, String)>,
    ) -> anyhow::Result<()> {
        // Get fc-runner managed runners from GitHub API
        let runners = if is_org {
            self.github.list_org_runners().await?
        } else if let Some(repo) = repos.first() {
            self.github.list_runners(repo).await?
        } else {
            return Ok(());
        };

        // Count only our runners (fc- prefix, online)
        let our_runners: Vec<_> = runners
            .iter()
            .filter(|r| r.name.starts_with("fc-") && r.status == "online")
            .collect();

        let total = our_runners.len();
        let busy = our_runners.iter().filter(|r| r.busy).count();
        let idle = total - busy;

        tracing::debug!(total, busy, idle, target_idle, max_vms, "autoscale check");

        // Spawn standbys if idle runners are below target and we have capacity
        if idle < target_idle {
            let active_vms = *self.active_jobs.lock().await;
            let to_spawn = (target_idle - idle).min(max_vms.saturating_sub(active_vms));

            if to_spawn == 0 {
                tracing::debug!(active_vms, max_vms, "at capacity, cannot spawn standbys");
                return Ok(());
            }

            tracing::info!(
                idle,
                busy,
                target_idle,
                spawning = to_spawn,
                "idle runners below target, spawning standbys"
            );

            for i in 0..to_spawn {
                let slot = {
                    let mut pool = self.slot_pool.lock().await;
                    match pool.pop() {
                        Some(s) => s,
                        None => {
                            tracing::debug!("no more slots for standby");
                            break;
                        }
                    }
                };
                let repo = if is_org {
                    String::new()
                } else {
                    repos[i % repos.len()].clone()
                };
                tracing::info!(slot, repo = %repo, "spawning standby runner");
                self.spawn_warm_vm(slot, repo, done_tx.clone());
            }
        }

        Ok(())
    }

    fn spawn_warm_vm(&self, slot: usize, repo: String, done_tx: mpsc::Sender<(usize, String)>) {
        let config = self.config.clone();
        let github = self.github.clone();
        let active_jobs = self.active_jobs.clone();
        let cancel = self.cancel.clone();
        let log_tx = self.server_state.log_tx.clone();

        tokio::spawn(async move {
            *active_jobs.lock().await += 1;
            metrics::JOBS_ACTIVE.inc();
            metrics::POOL_SLOTS_AVAILABLE.dec();
            tracing::info!(slot, repo = %repo, "starting warm pool VM");

            let timer = metrics::VM_BOOT_DURATION
                .with_label_values(&[&repo])
                .start_timer();

            // Create a VSOCK notification channel so the orchestrator can start
            // a replacement as soon as the guest agent reports job completion,
            // before the VM fully shuts down.
            let (vsock_tx, mut vsock_rx) =
                mpsc::channel::<crate::vm::vsock::JobDoneNotification>(1);
            let early_done_tx = done_tx.clone();
            let early_repo = repo.clone();
            let early_slot = slot;

            // Listen for early completion signal from VSOCK
            let early_handle = tokio::spawn(async move {
                if let Some(notification) = vsock_rx.recv().await {
                    tracing::info!(
                        slot = early_slot,
                        vm_id = %notification.vm_id,
                        exit_code = notification.exit_code,
                        "VSOCK: job completed, signaling early replacement"
                    );
                    // Signal the warm pool to start creating a replacement immediately
                    let _ = early_done_tx.send((early_slot, early_repo)).await;
                    true
                } else {
                    tracing::debug!(
                        slot = early_slot,
                        "VSOCK channel closed without notification"
                    );
                    false
                }
            });

            tracing::info!(slot, repo = %repo, "warm pool VM running, waiting for exit...");
            let ctx = VmRunContext::new(config, github.clone(), slot, cancel)
                .log_tx(log_tx)
                .vsock_notify(vsock_tx);
            let result = run_warm_vm(ctx, &repo).await;
            timer.observe_duration();
            tracing::info!(
                slot,
                repo = %repo,
                success = result.is_ok(),
                "warm pool VM exited"
            );

            // Check if VSOCK already sent the early replacement signal.
            // Use a timeout to prevent deadlock if the VSOCK channel isn't
            // cleaned up properly (e.g. listener task stuck on accept).
            let early_signaled =
                match tokio::time::timeout(Duration::from_secs(5), early_handle).await {
                    Ok(Ok(signaled)) => {
                        tracing::debug!(slot, early_signaled = signaled, "early_handle resolved");
                        signaled
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(slot, error = %e, "early_handle task panicked");
                        false
                    }
                    Err(_) => {
                        tracing::warn!(
                            slot,
                            "early_handle timed out after 5s, forcing replacement signal"
                        );
                        false
                    }
                };

            // Delete this specific runner from GitHub (targeted, not a full scan)
            match &result {
                Ok(vm_result) => {
                    metrics::JOBS_COMPLETED.with_label_values(&[&repo]).inc();
                    tracing::info!(
                        slot,
                        repo = %repo,
                        runner_name = %vm_result.runner_name,
                        "warm pool VM completed, deleting runner from GitHub"
                    );
                    if github.is_org_mode() {
                        github
                            .delete_org_runner_by_name(&vm_result.runner_name)
                            .await;
                    } else {
                        github
                            .delete_runner_by_name(&repo, &vm_result.runner_name)
                            .await;
                    }
                    tracing::info!(
                        slot,
                        runner_name = %vm_result.runner_name,
                        "runner deletion complete"
                    );
                }
                Err(e) => {
                    metrics::JOBS_FAILED.with_label_values(&[&repo]).inc();
                    tracing::error!(slot, repo = %repo, error = ?e, "warm pool VM failed");
                    tracing::info!(slot, "falling back to offline runner scan");
                    // Fall back to scanning for offline runners since we don't have the name
                    if github.is_org_mode() {
                        github.remove_org_offline_runners().await;
                    } else {
                        github.remove_offline_runners(&repo).await;
                    }
                }
            }

            *active_jobs.lock().await -= 1;
            metrics::JOBS_ACTIVE.dec();
            metrics::POOL_SLOTS_AVAILABLE.inc();

            // Only signal done_tx if VSOCK didn't already send an early signal
            if !early_signaled {
                tracing::info!(
                    slot,
                    repo = %repo,
                    "signaling warm pool for replacement VM"
                );
                match done_tx.send((slot, repo)).await {
                    Ok(()) => {
                        tracing::info!(slot, "replacement signal sent successfully");
                    }
                    Err(e) => {
                        tracing::error!(
                            slot,
                            error = %e,
                            "failed to send replacement signal (receiver dropped?)"
                        );
                    }
                }
            } else {
                tracing::info!(
                    slot,
                    "skipping replacement signal (VSOCK already triggered early replacement)"
                );
            }
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
                tracing::warn!(
                    remaining = count,
                    "shutdown timeout, some jobs still running"
                );
                break;
            }
            tracing::info!(remaining = count, "waiting for in-flight jobs...");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

// ── Job runners ──────────────────────────────────────────────────────

async fn run_jit_job(ctx: VmRunContext, job_id: u64, repo: &str) -> anyhow::Result<()> {
    tracing::info!(job_id, repo = %repo, slot = ctx.slot, "requesting JIT token");
    let jit_token = ctx.github.generate_jit_config(repo, job_id).await?;
    tracing::info!(job_id, repo = %repo, "JIT token acquired");

    let repo_url = format!("https://github.com/{}/{}", ctx.config.github.owner, repo);
    let mut vm = MicroVm::new(
        job_id,
        &ctx.config.firecracker,
        &ctx.config.network,
        &ctx.config.runner.work_dir,
        ctx.config.runner.vm_timeout_secs,
        ctx.slot,
        ctx.cancel.clone(),
    );
    if ctx.config.cache_service.enabled {
        vm.cache_service_token = ctx.config.cache_service.token.clone();
        vm.cache_service_port = ctx
            .config
            .server
            .listen_addr
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok());
    }
    let ephemeral = ctx.config.runner.ephemeral;
    let mut env_content = format!(
        "RUNNER_MODE=jit\nRUNNER_TOKEN={}\nREPO_URL={}\nVM_ID={}\nRUNNER_JIT_CONFIG={}\nHOSTNAME={}\nSHUTDOWN_ON_EXIT=true\nEPHEMERAL={}\n",
        jit_token, repo_url, vm.vm_id, jit_token, vm.vm_id, ephemeral
    );
    append_cache_env(&ctx.config, &mut vm, &mut env_content);
    vm.execute(&env_content, ctx).await
}

/// Result of running a warm pool VM: the runner name registered on GitHub.
struct WarmVmResult {
    runner_name: String,
}

async fn run_warm_vm(ctx: VmRunContext, repo: &str) -> anyhow::Result<WarmVmResult> {
    let is_org = ctx.github.is_org_mode();
    let slot = ctx.slot;

    let (reg_token, registration_url) = if is_org {
        let org = ctx
            .config
            .github
            .organization
            .as_deref()
            .unwrap_or("unknown");
        tracing::info!(
            slot,
            organization = org,
            "requesting org registration token"
        );
        let token = ctx.github.generate_org_registration_token().await?;
        let url = format!("https://github.com/{}", org);
        tracing::info!(slot, organization = org, "org registration token acquired");
        (token, url)
    } else {
        tracing::info!(slot, repo = %repo, "requesting registration token");
        let token = ctx.github.generate_registration_token(repo).await?;
        let url = format!("https://github.com/{}/{}", ctx.config.github.owner, repo);
        tracing::info!(slot, repo = %repo, "registration token acquired");
        (token, url)
    };

    let runner_name = format!(
        "fc-warm-{}-{}",
        slot,
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    tracing::info!(
        slot,
        runner_name = %runner_name,
        "registering warm pool runner"
    );
    let mut vm = MicroVm::new(
        0, // no specific job_id for warm pool VMs
        &ctx.config.firecracker,
        &ctx.config.network,
        &ctx.config.runner.work_dir,
        ctx.config.runner.vm_timeout_secs,
        slot,
        ctx.cancel.clone(),
    );
    if ctx.config.cache_service.enabled {
        vm.cache_service_token = ctx.config.cache_service.token.clone();
        vm.cache_service_port = ctx
            .config
            .server
            .listen_addr
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok());
    }
    let ephemeral = ctx.config.runner.ephemeral;
    let mut env_content = format!(
        "RUNNER_MODE=register\nRUNNER_TOKEN={}\nREPO_URL={}\nRUNNER_NAME={}\nVM_ID={}\nHOSTNAME={}\nSHUTDOWN_ON_EXIT=true\nEPHEMERAL={}\n",
        reg_token, registration_url, runner_name, vm.vm_id, vm.vm_id, ephemeral
    );
    append_cache_env(&ctx.config, &mut vm, &mut env_content);
    tracing::info!(
        slot,
        vm_id = %vm.vm_id,
        runner_name = %runner_name,
        ephemeral,
        "launching warm pool VM"
    );
    vm.execute(&env_content, ctx).await?;
    tracing::info!(
        slot,
        runner_name = %runner_name,
        "warm pool VM execution finished"
    );
    Ok(WarmVmResult { runner_name })
}

/// Append cache service env vars to env_content when the cache service is enabled.
/// These flow through MMDS to the guest agent, which passes them to the runner process
/// as ACTIONS_CACHE_URL, ACTIONS_RUNTIME_TOKEN, and S3 credentials for runs-on/cache.
fn append_cache_env(config: &AppConfig, vm: &mut MicroVm, env_content: &mut String) {
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

        // S3 credentials for runs-on/cache direct uploads.
        // Rewrite localhost → host IP so URLs are reachable from the VM.
        let s3_endpoint = config
            .cache_service
            .s3_endpoint
            .replace("localhost", &vm.host_ip)
            .replace("127.0.0.1", &vm.host_ip);
        env_content.push_str(&format!("S3_ENDPOINT={}\n", s3_endpoint.clone()));
        env_content.push_str(&format!("S3_BUCKET={}\n", config.cache_service.s3_bucket));
        env_content.push_str(&format!("S3_REGION={}\n", config.cache_service.s3_region));
        if let Some(key) = &config.cache_service.s3_access_key {
            env_content.push_str(&format!("S3_ACCESS_KEY={}\n", key));
        }
        if let Some(key) = &config.cache_service.s3_secret_key {
            env_content.push_str(&format!("S3_SECRET_KEY={}\n", key));
        }

        // Also set structured S3 config on the VM for mount-mode injection
        if let (Some(ak), Some(sk)) = (
            &config.cache_service.s3_access_key,
            &config.cache_service.s3_secret_key,
        ) {
            vm.s3_config = Some(crate::vm::firecracker::S3GuestConfig {
                endpoint: s3_endpoint,
                bucket: config.cache_service.s3_bucket.clone(),
                access_key: ak.clone(),
                secret_key: sk.clone(),
                region: config.cache_service.s3_region.clone(),
            });
        }
    }
}
