use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::Mutex;
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
}

impl Orchestrator {
    pub fn new(config: Arc<AppConfig>, cancel: CancellationToken) -> anyhow::Result<Self> {
        let github = Arc::new(GitHubClient::new(config.github.clone())?);
        Ok(Self {
            config,
            github,
            seen_jobs: Arc::new(Mutex::new(HashSet::new())),
            cancel,
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
                    tracing::info!("shutdown signal received, stopping poll loop");
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
        let runs = self.github.list_queued_runs().await?;
        tracing::debug!(count = runs.len(), "found queued runs");

        for run in runs {
            let jobs = self.github.list_queued_jobs(run.id).await?;
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

                tracing::info!(job_id = job.id, run_id = job.run_id, "dispatching new job");
                self.dispatch_job(job.id);
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

    fn dispatch_job(&self, job_id: u64) {
        let config = self.config.clone();
        let github = self.github.clone();
        let seen_jobs = self.seen_jobs.clone();

        tokio::spawn(async move {
            if let Err(e) = run_job(config.clone(), github, job_id).await {
                tracing::error!(job_id, error = %e, "job failed");
            }
            seen_jobs.lock().await.remove(&job_id);
        });
    }
}

async fn run_job(
    config: Arc<AppConfig>,
    github: Arc<GitHubClient>,
    job_id: u64,
) -> anyhow::Result<()> {
    let jit_token = github.generate_jit_config(job_id).await?;
    let repo_url = format!(
        "https://github.com/{}/{}",
        config.github.owner, config.github.repo
    );
    let vm = MicroVm::new(job_id, &config.firecracker, &config.runner.work_dir);
    vm.execute(&jit_token, &repo_url).await
}
