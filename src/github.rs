use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::Context;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde::Deserialize;
use tokio::time::Duration;

use crate::config::GitHubConfig;

#[derive(Debug, Deserialize)]
pub struct WorkflowRunsResponse {
    pub workflow_runs: Vec<WorkflowRun>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
}

#[derive(Debug, Deserialize)]
pub struct JobsResponse {
    pub jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
pub struct Job {
    pub id: u64,
    pub run_id: u64,
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct JitConfigResponse {
    pub encoded_jit_config: String,
}

#[derive(Debug, Deserialize)]
pub struct RunnersResponse {
    pub runners: Vec<Runner>,
}

#[derive(Debug, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub status: String,
}

pub struct GitHubClient {
    client: Client,
    config: GitHubConfig,
    rate_limit_remaining: Arc<AtomicU32>,
}

impl GitHubClient {
    pub fn new(config: GitHubConfig) -> anyhow::Result<Self> {
        let client = Client::builder()
            .user_agent("fc-runner/0.1")
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            config,
            rate_limit_remaining: Arc::new(AtomicU32::new(5000)),
        })
    }

    /// Returns the base API URL for a given repo.
    fn repo_url(&self, repo: &str) -> String {
        format!(
            "https://api.github.com/repos/{}/{}",
            self.config.owner, repo
        )
    }

    /// Returns the list of repos to poll.
    pub fn repos(&self) -> Vec<String> {
        self.config.all_repos()
    }

    /// Returns the owner name.
    pub fn owner(&self) -> &str {
        &self.config.owner
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(self.config.token.expose_secret())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// Check rate limit headers and warn/backoff if running low.
    async fn check_rate_limit(&self, resp: &reqwest::Response) {
        if let Some(remaining) = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
        {
            self.rate_limit_remaining.store(remaining, Ordering::Relaxed);
            if remaining < 100 {
                tracing::warn!(remaining, "GitHub API rate limit running low");
            }
            if remaining < 10 {
                tracing::error!(remaining, "GitHub API rate limit nearly exhausted, backing off");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    }

    pub async fn list_queued_runs(&self, repo: &str) -> anyhow::Result<Vec<WorkflowRun>> {
        let url = format!("{}/actions/runs?status=queued", self.repo_url(repo));
        let resp = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let data = resp
            .error_for_status()
            .with_context(|| format!("listing queued runs for {}/{}", self.config.owner, repo))?
            .json::<WorkflowRunsResponse>()
            .await?;
        Ok(data.workflow_runs)
    }

    pub async fn list_queued_jobs(&self, repo: &str, run_id: u64) -> anyhow::Result<Vec<Job>> {
        let url = format!(
            "{}/actions/runs/{}/jobs?filter=queued",
            self.repo_url(repo),
            run_id
        );
        let resp = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let data = resp
            .error_for_status()
            .with_context(|| format!("listing queued jobs for {}/{} run {}", self.config.owner, repo, run_id))?
            .json::<JobsResponse>()
            .await?;
        Ok(data.jobs)
    }

    /// Generate a registration token for pre-registering runners (warm pool mode).
    pub async fn generate_registration_token(&self, repo: &str) -> anyhow::Result<String> {
        let url = format!("{}/actions/runners/registration-token", self.repo_url(repo));
        let resp = self
            .request(reqwest::Method::POST, &url)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "registration token for {}/{} failed (HTTP {}): {}",
                self.config.owner, repo, status, body
            );
        }
        #[derive(Deserialize)]
        struct RegToken {
            token: String,
        }
        let data = resp.json::<RegToken>().await?;
        Ok(data.token)
    }

    /// List all self-hosted runners for a repo.
    pub async fn list_runners(&self, repo: &str) -> anyhow::Result<Vec<Runner>> {
        let url = format!("{}/actions/runners", self.repo_url(repo));
        let resp = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let data = resp
            .error_for_status()
            .with_context(|| format!("listing runners for {}/{}", self.config.owner, repo))?
            .json::<RunnersResponse>()
            .await?;
        Ok(data.runners)
    }

    /// Delete a runner by ID.
    pub async fn delete_runner(&self, repo: &str, runner_id: u64) -> anyhow::Result<()> {
        let url = format!("{}/actions/runners/{}", self.repo_url(repo), runner_id);
        let resp = self
            .request(reqwest::Method::DELETE, &url)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "delete runner {} for {}/{} failed (HTTP {}): {}",
                runner_id, self.config.owner, repo, status, body
            );
        }
        Ok(())
    }

    /// Remove offline runners whose names start with "fc-" (left over from completed/crashed VMs).
    pub async fn remove_offline_runners(&self, repo: &str) {
        match self.list_runners(repo).await {
            Ok(runners) => {
                for runner in runners {
                    if runner.name.starts_with("fc-") && runner.status == "offline" {
                        tracing::info!(
                            runner_id = runner.id,
                            runner_name = %runner.name,
                            repo = %repo,
                            "removing offline runner"
                        );
                        if let Err(e) = self.delete_runner(repo, runner.id).await {
                            tracing::warn!(
                                runner_id = runner.id,
                                error = %e,
                                "failed to remove offline runner"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "failed to list runners for cleanup");
            }
        }
    }

    pub async fn generate_jit_config(&self, repo: &str, job_id: u64) -> anyhow::Result<String> {
        let url = format!("{}/actions/runners/generate-jitconfig", self.repo_url(repo));
        let body = serde_json::json!({
            "name": format!("fc-{}-{}", job_id, &uuid::Uuid::new_v4().to_string()[..8]),
            "runner_group_id": self.config.runner_group_id,
            "labels": self.config.labels,
            "work_folder": "_work"
        });
        let resp = self
            .request(reqwest::Method::POST, &url)
            .json(&body)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "JIT config for {}/{} job {} failed (HTTP {}): {}",
                self.config.owner, repo, job_id, status, body
            );
        }
        let data = resp.json::<JitConfigResponse>().await?;
        Ok(data.encoded_jit_config)
    }
}
