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

pub struct GitHubClient {
    client: Client,
    config: GitHubConfig,
    base_url: String,
    rate_limit_remaining: Arc<AtomicU32>,
}

impl GitHubClient {
    pub fn new(config: GitHubConfig) -> anyhow::Result<Self> {
        let client = Client::builder()
            .user_agent("fc-runner/0.1")
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        let base_url = format!(
            "https://api.github.com/repos/{}/{}",
            config.owner, config.repo
        );
        Ok(Self {
            client,
            config,
            base_url,
            rate_limit_remaining: Arc::new(AtomicU32::new(5000)),
        })
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

    pub async fn list_queued_runs(&self) -> anyhow::Result<Vec<WorkflowRun>> {
        let url = format!("{}/actions/runs?status=queued", self.base_url);
        let resp = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let data = resp
            .error_for_status()
            .context("listing queued runs")?
            .json::<WorkflowRunsResponse>()
            .await?;
        Ok(data.workflow_runs)
    }

    pub async fn list_queued_jobs(&self, run_id: u64) -> anyhow::Result<Vec<Job>> {
        let url = format!(
            "{}/actions/runs/{}/jobs?filter=queued",
            self.base_url, run_id
        );
        let resp = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let data = resp
            .error_for_status()
            .context("listing queued jobs")?
            .json::<JobsResponse>()
            .await?;
        Ok(data.jobs)
    }

    pub async fn generate_jit_config(&self, job_id: u64) -> anyhow::Result<String> {
        let url = format!("{}/actions/runners/generate-jit-config", self.base_url);
        let body = serde_json::json!({
            "name": format!("fc-{}", job_id),
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
        let data = resp
            .error_for_status()
            .context("generating JIT config")?
            .json::<JitConfigResponse>()
            .await?;
        Ok(data.encoded_jit_config)
    }
}
