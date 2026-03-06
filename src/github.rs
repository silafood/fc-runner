use anyhow::Context;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde::Deserialize;

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
}

impl GitHubClient {
    pub fn new(config: GitHubConfig) -> anyhow::Result<Self> {
        let client = Client::builder()
            .user_agent("fc-runner/0.1")
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
        })
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(self.config.token.expose_secret())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    pub async fn list_queued_runs(&self) -> anyhow::Result<Vec<WorkflowRun>> {
        let url = format!("{}/actions/runs?status=queued", self.base_url);
        let resp = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await?
            .error_for_status()
            .context("listing queued runs")?
            .json::<WorkflowRunsResponse>()
            .await?;
        Ok(resp.workflow_runs)
    }

    pub async fn list_queued_jobs(&self, run_id: u64) -> anyhow::Result<Vec<Job>> {
        let url = format!(
            "{}/actions/runs/{}/jobs?filter=queued",
            self.base_url, run_id
        );
        let resp = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await?
            .error_for_status()
            .context("listing queued jobs")?
            .json::<JobsResponse>()
            .await?;
        Ok(resp.jobs)
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
            .await?
            .error_for_status()
            .context("generating JIT config")?
            .json::<JitConfigResponse>()
            .await?;
        Ok(resp.encoded_jit_config)
    }
}
