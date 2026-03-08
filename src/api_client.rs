use anyhow::{bail, Context};
use serde::Deserialize;

pub struct ApiClient {
    base_url: String,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
pub struct VmInfo {
    pub vm_id: String,
    pub job_id: u64,
    pub repo: String,
    pub slot: usize,
    pub started_at: String,
}

#[derive(Debug, Deserialize)]
pub struct StatusInfo {
    pub version: String,
    pub uptime_seconds: u64,
    pub mode: String,
    pub active_vms: usize,
}

impl ApiClient {
    pub fn new(endpoint: &str) -> Self {
        Self {
            base_url: endpoint.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn status(&self) -> anyhow::Result<StatusInfo> {
        let resp = self
            .client
            .get(format!("{}/api/v1/status", self.base_url))
            .send()
            .await
            .context("failed to connect to server")?;
        if !resp.status().is_success() {
            bail!("server returned {}", resp.status());
        }
        resp.json().await.context("failed to parse response")
    }

    pub async fn list_vms(&self) -> anyhow::Result<Vec<VmInfo>> {
        let resp = self
            .client
            .get(format!("{}/api/v1/vms", self.base_url))
            .send()
            .await
            .context("failed to connect to server")?;
        if !resp.status().is_success() {
            bail!("server returned {}", resp.status());
        }
        resp.json().await.context("failed to parse response")
    }
}
