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
#[allow(dead_code)]
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

    #[allow(dead_code)]
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

    #[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_client_strips_trailing_slash() {
        let client = ApiClient::new("http://localhost:9090/");
        assert_eq!(client.base_url, "http://localhost:9090");
    }

    #[test]
    fn api_client_preserves_clean_url() {
        let client = ApiClient::new("http://localhost:9090");
        assert_eq!(client.base_url, "http://localhost:9090");
    }

    #[test]
    fn api_client_handles_custom_port() {
        let client = ApiClient::new("http://192.168.1.100:8080");
        assert_eq!(client.base_url, "http://192.168.1.100:8080");
    }

    #[test]
    fn vm_info_deserialize() {
        let json = r#"{"vm_id":"fc-1-slot0","job_id":1,"repo":"test","slot":0,"started_at":"12345"}"#;
        let info: VmInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.vm_id, "fc-1-slot0");
        assert_eq!(info.job_id, 1);
        assert_eq!(info.repo, "test");
        assert_eq!(info.slot, 0);
        assert_eq!(info.started_at, "12345");
    }

    #[test]
    fn status_info_deserialize() {
        let json = r#"{"version":"0.1.0","uptime_seconds":120,"mode":"jit","active_vms":2}"#;
        let info: StatusInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.version, "0.1.0");
        assert_eq!(info.uptime_seconds, 120);
        assert_eq!(info.mode, "jit");
        assert_eq!(info.active_vms, 2);
    }

    #[test]
    fn pool_status_deserialize() {
        let json = r#"{"name":"default","repos":["a","b"],"min_ready":2,"max_ready":4,"active":1,"idle_slots":3,"paused":false}"#;
        let status: PoolStatus = serde_json::from_str(json).unwrap();
        assert_eq!(status.name, "default");
        assert_eq!(status.repos, vec!["a", "b"]);
        assert_eq!(status.min_ready, 2);
        assert_eq!(status.max_ready, 4);
        assert_eq!(status.active, 1);
        assert_eq!(status.idle_slots, 3);
        assert!(!status.paused);
    }

    #[test]
    fn pool_status_paused() {
        let json = r#"{"name":"test","repos":[],"min_ready":0,"max_ready":0,"active":0,"idle_slots":0,"paused":true}"#;
        let status: PoolStatus = serde_json::from_str(json).unwrap();
        assert!(status.paused);
    }

    #[test]
    fn action_response_deserialize() {
        let json = r#"{"message":"pool paused"}"#;
        let resp: ActionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message, "pool paused");
    }

    #[test]
    fn scale_request_serialize_full() {
        let req = ScaleRequest {
            min_ready: Some(3),
            max_ready: Some(8),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""min_ready":3"#));
        assert!(json.contains(r#""max_ready":8"#));
    }

    #[test]
    fn scale_request_serialize_partial() {
        let req = ScaleRequest {
            min_ready: Some(5),
            max_ready: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""min_ready":5"#));
        assert!(!json.contains("max_ready"));
    }

    #[test]
    fn scale_request_serialize_empty() {
        let req = ScaleRequest {
            min_ready: None,
            max_ready: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, "{}");
    }

    #[tokio::test]
    async fn api_client_connection_refused() {
        let client = ApiClient::new("http://127.0.0.1:1");
        let result = client.status().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("connect"));
    }

    #[tokio::test]
    async fn api_client_list_vms_connection_refused() {
        let client = ApiClient::new("http://127.0.0.1:1");
        let result = client.list_vms().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn api_client_list_pools_connection_refused() {
        let client = ApiClient::new("http://127.0.0.1:1");
        let result = client.list_pools().await;
        assert!(result.is_err());
    }
}
