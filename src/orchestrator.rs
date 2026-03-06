use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore};
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::firecracker::MicroVm;
use crate::github::GitHubClient;

pub struct Orchestrator {
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    seen_jobs: Arc<Mutex<HashSet<u64>>>,
    cancel: CancellationToken,
    semaphore: Arc<Semaphore>,
    active_jobs: Arc<Mutex<usize>>,
}

impl Orchestrator {
    pub fn new(config: Arc<AppConfig>, cancel: CancellationToken) -> anyhow::Result<Self> {
        let github = Arc::new(GitHubClient::new(config.github.clone())?);
        let max_jobs = config.runner.max_concurrent_jobs;
        let repos = github.repos();
        tracing::info!(
            max_concurrent_jobs = max_jobs,
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
        })
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let mut ticker = interval(Duration::from_secs(self.config.runner.poll_interval_secs));
        tracing::info!(
            poll_interval = self.config.runner.poll_interval_secs,
            "orchestrator starting"
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

    async fn poll_once(&self) -> anyhow::Result<()> {
        for repo in self.github.repos() {
            if let Err(e) = self.poll_repo(&repo).await {
                tracing::error!(
                    repo = %repo,
                    error = %e,
                    "failed to poll repo, skipping"
                );
            }
        }
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
                self.dispatch_job(job.id, repo.to_string());
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

    fn dispatch_job(&self, job_id: u64, repo: String) {
        let config = self.config.clone();
        let github = self.github.clone();
        let seen_jobs = self.seen_jobs.clone();
        let semaphore = self.semaphore.clone();
        let active_jobs = self.active_jobs.clone();

        tokio::spawn(async move {
            // Acquire concurrency permit — blocks if at max capacity
            let _permit = match semaphore.acquire().await {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::error!(job_id, "semaphore closed, cannot dispatch job");
                    return;
                }
            };

            *active_jobs.lock().await += 1;
            tracing::info!(job_id, repo = %repo, "job started (permit acquired)");

            if let Err(e) = run_job(config.clone(), github, job_id, &repo).await {
                tracing::error!(job_id, repo = %repo, error = %e, "job failed");
            }

            *active_jobs.lock().await -= 1;
            seen_jobs.lock().await.remove(&job_id);
            tracing::info!(job_id, repo = %repo, "job completed (permit released)");
        });
    }
}

async fn run_job(
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    job_id: u64,
    repo: &str,
) -> anyhow::Result<()> {
    tracing::info!(job_id, repo = %repo, "requesting JIT token");
    let jit_token = github.generate_jit_config(repo, job_id).await?;
    tracing::info!(job_id, repo = %repo, "JIT token acquired");

    let repo_url = format!(
        "https://github.com/{}/{}",
        config.github.owner, repo
    );
    let vm = MicroVm::new(job_id, &config.firecracker, &config.runner.work_dir, config.runner.vm_timeout_secs);
    vm.execute(&jit_token, &repo_url).await
}
