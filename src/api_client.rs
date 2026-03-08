use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Deserialize)]
pub struct PoolStatus {
    pub name: String,
    pub repos: Vec<String>,
    pub min_ready: usize,
    pub max_ready: usize,
    pub active: usize,
    pub idle_slots: usize,
    pub paused: bool,
}

#[derive(Debug, Deserialize)]
pub struct ActionResponse {
    pub message: String,
}

#[derive(Serialize)]
struct ScaleRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    min_ready: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_ready: Option<usize>,
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

    pub async fn list_pools(&self) -> anyhow::Result<Vec<PoolStatus>> {
        let resp = self
            .client
            .get(format!("{}/api/v1/pools", self.base_url))
            .send()
            .await
            .context("failed to connect to server")?;
        if !resp.status().is_success() {
            bail!("server returned {}", resp.status());
        }
        resp.json().await.context("failed to parse response")
    }

    pub async fn get_pool(&self, name: &str) -> anyhow::Result<PoolStatus> {
        let resp = self
            .client
            .get(format!("{}/api/v1/pools/{}", self.base_url, name))
            .send()
            .await
            .context("failed to connect to server")?;
        if !resp.status().is_success() {
            bail!("server returned {}", resp.status());
        }
        resp.json().await.context("failed to parse response")
    }

    pub async fn scale_pool(
        &self,
        name: &str,
        min_ready: Option<usize>,
        max_ready: Option<usize>,
    ) -> anyhow::Result<ActionResponse> {
        let resp = self
            .client
            .post(format!("{}/api/v1/pools/{}/scale", self.base_url, name))
            .json(&ScaleRequest { min_ready, max_ready })
            .send()
            .await
            .context("failed to connect to server")?;
        if !resp.status().is_success() {
            bail!("server returned {}", resp.status());
        }
        resp.json().await.context("failed to parse response")
    }

    pub async fn pause_pool(&self, name: &str) -> anyhow::Result<ActionResponse> {
        let resp = self
            .client
            .post(format!("{}/api/v1/pools/{}/pause", self.base_url, name))
            .send()
            .await
            .context("failed to connect to server")?;
        if !resp.status().is_success() {
            bail!("server returned {}", resp.status());
        }
        resp.json().await.context("failed to parse response")
    }

    pub async fn resume_pool(&self, name: &str) -> anyhow::Result<ActionResponse> {
        let resp = self
            .client
            .post(format!("{}/api/v1/pools/{}/resume", self.base_url, name))
            .send()
            .await
            .context("failed to connect to server")?;
        if !resp.status().is_success() {
            bail!("server returned {}", resp.status());
        }
        resp.json().await.context("failed to parse response")
    }
}
