//! Guest agent — runs inside a Firecracker VM.
//!
//! Reads MMDS metadata, sets hostname, starts the GitHub Actions runner,
//! and reports state to the host via VSOCK (NDJSON on port 1024).

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

/// MMDS V2 base URL (Firecracker metadata service).
const MMDS_BASE: &str = "http://169.254.169.254";
/// MMDS token TTL in seconds.
const MMDS_TOKEN_TTL: u32 = 300;
/// VSOCK host CID (always 2 per Firecracker spec).
#[cfg(target_os = "linux")]
const HOST_CID: u32 = 2;
/// VSOCK port for agent communication.
#[cfg(target_os = "linux")]
const AGENT_PORT: u32 = 1024;
/// Path to the actions runner directory.
const RUNNER_DIR: &str = "/home/runner/actions-runner";

/// Metadata expected from MMDS.
#[derive(Debug, Deserialize)]
struct Metadata {
    runner_jit_config: String,
    hostname: String,
    #[serde(default)]
    shutdown_on_exit: bool,
}

/// Messages sent from agent to host via VSOCK.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum AgentMessage {
    Ready { timestamp: String },
    JobStarted { job_id: Option<u64> },
    Log { line: String },
    JobCompleted { exit_code: i32 },
}

/// Run the guest agent.
pub async fn run(log_level: &str) -> anyhow::Result<()> {
    let filter = format!("fc_runner={}", log_level);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(filter.parse().unwrap_or_else(|_| "info".parse().unwrap())),
        )
        .init();

    tracing::info!("fc-runner agent starting");

    let metadata = read_mmds_with_retry(10, Duration::from_secs(1)).await?;
    tracing::info!(hostname = %metadata.hostname, "MMDS metadata loaded");

    set_hostname(&metadata.hostname).await;

    // On Linux, connect VSOCK and report state; on other platforms, just run the runner
    let exit_code = run_with_reporting(&metadata).await;

    if metadata.shutdown_on_exit {
        tracing::info!("shutting down VM");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = tokio::process::Command::new("poweroff")
            .arg("-f")
            .status()
            .await;
    }

    if exit_code != 0 {
        bail!("runner exited with code {}", exit_code);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn run_with_reporting(metadata: &Metadata) -> i32 {
    use tokio::io::AsyncWriteExt;

    let addr = tokio_vsock::VsockAddr::new(HOST_CID, AGENT_PORT);
    let mut stream = match tokio_vsock::VsockStream::connect(addr).await {
        Ok(s) => {
            tracing::info!("connected to host via VSOCK");
            Some(s)
        }
        Err(e) => {
            tracing::warn!(error = %e, "VSOCK connection failed, running without host reporting");
            None
        }
    };

    vsock_send(&mut stream, &AgentMessage::Ready {
        timestamp: chrono::Utc::now().to_rfc3339(),
    }).await;

    tracing::info!("starting GitHub Actions runner");
    vsock_send(&mut stream, &AgentMessage::JobStarted { job_id: None }).await;

    let exit_code = run_runner(&metadata.runner_jit_config).await;

    tracing::info!(exit_code, "runner exited");
    vsock_send(&mut stream, &AgentMessage::JobCompleted { exit_code }).await;

    // Flush before closing
    if let Some(s) = &mut stream {
        let _ = s.flush().await;
    }

    exit_code
}

#[cfg(not(target_os = "linux"))]
async fn run_with_reporting(metadata: &Metadata) -> i32 {
    tracing::warn!("VSOCK not available on this platform, running without host reporting");
    tracing::info!("starting GitHub Actions runner");
    let exit_code = run_runner(&metadata.runner_jit_config).await;
    tracing::info!(exit_code, "runner exited");
    exit_code
}

#[cfg(target_os = "linux")]
async fn vsock_send(stream: &mut Option<tokio_vsock::VsockStream>, msg: &AgentMessage) {
    use tokio::io::AsyncWriteExt;
    if let Some(s) = stream.as_mut() {
        let mut line = serde_json::to_string(msg).unwrap_or_default();
        line.push('\n');
        if let Err(e) = s.write_all(line.as_bytes()).await {
            tracing::warn!(error = %e, "VSOCK write failed");
        }
    }
}

/// Read MMDS metadata with retries.
async fn read_mmds_with_retry(max_attempts: u32, delay: Duration) -> anyhow::Result<Metadata> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("building HTTP client")?;

    for attempt in 1..=max_attempts {
        match read_mmds(&client).await {
            Ok(meta) => return Ok(meta),
            Err(e) => {
                if attempt == max_attempts {
                    return Err(e).context("failed to read MMDS after all retries");
                }
                tracing::warn!(attempt, error = %e, "MMDS read failed, retrying...");
                tokio::time::sleep(delay).await;
            }
        }
    }
    unreachable!()
}

/// Read MMDS V2 metadata (acquire token, then GET metadata).
async fn read_mmds(client: &reqwest::Client) -> anyhow::Result<Metadata> {
    let token = client
        .put(format!("{}/latest/api/token", MMDS_BASE))
        .header("X-metadata-token-ttl-seconds", MMDS_TOKEN_TTL.to_string())
        .send()
        .await
        .context("requesting MMDS token")?
        .text()
        .await
        .context("reading MMDS token")?;

    let resp = client
        .get(format!("{}/fc-runner", MMDS_BASE))
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .send()
        .await
        .context("requesting MMDS metadata")?;

    if !resp.status().is_success() {
        bail!("MMDS GET /fc-runner returned HTTP {}", resp.status());
    }

    let metadata: Metadata = resp.json().await.context("parsing MMDS metadata JSON")?;
    Ok(metadata)
}

/// Set the system hostname.
async fn set_hostname(hostname: &str) {
    if let Err(e) = tokio::process::Command::new("hostname")
        .arg(hostname)
        .status()
        .await
    {
        tracing::warn!(error = %e, "failed to set hostname");
    }
}

/// Run the GitHub Actions runner and return its exit code.
async fn run_runner(jit_config: &str) -> i32 {
    let run_sh = format!("{}/run.sh", RUNNER_DIR);

    let mut child = match tokio::process::Command::new(&run_sh)
        .arg("--jitconfig")
        .arg(jit_config)
        .current_dir(RUNNER_DIR)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to start runner");
            return 1;
        }
    };

    match child.wait().await {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            tracing::error!(error = %e, "runner wait failed");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_deserialization() {
        let json = r#"{"runner_jit_config":"abc123","hostname":"fc-42","shutdown_on_exit":true}"#;
        let meta: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.runner_jit_config, "abc123");
        assert_eq!(meta.hostname, "fc-42");
        assert!(meta.shutdown_on_exit);
    }

    #[test]
    fn metadata_shutdown_defaults_false() {
        let json = r#"{"runner_jit_config":"token","hostname":"fc-0"}"#;
        let meta: Metadata = serde_json::from_str(json).unwrap();
        assert!(!meta.shutdown_on_exit);
    }

    #[test]
    fn metadata_missing_required_fields_fails() {
        let json = r#"{"hostname":"fc-0"}"#;
        let result = serde_json::from_str::<Metadata>(json);
        assert!(result.is_err());
    }

    #[test]
    fn agent_message_ready_serialization() {
        let msg = AgentMessage::Ready {
            timestamp: "2024-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"ready""#));
        assert!(json.contains(r#""timestamp":"2024-01-01T00:00:00Z""#));
    }

    #[test]
    fn agent_message_job_started_serialization() {
        let msg = AgentMessage::JobStarted { job_id: Some(42) };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"job_started""#));
        assert!(json.contains(r#""job_id":42"#));
    }

    #[test]
    fn agent_message_job_started_no_id() {
        let msg = AgentMessage::JobStarted { job_id: None };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"job_started""#));
        assert!(json.contains(r#""job_id":null"#));
    }

    #[test]
    fn agent_message_log_serialization() {
        let msg = AgentMessage::Log {
            line: "hello world".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"log""#));
        assert!(json.contains(r#""line":"hello world""#));
    }

    #[test]
    fn agent_message_job_completed_serialization() {
        let msg = AgentMessage::JobCompleted { exit_code: 0 };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"job_completed""#));
        assert!(json.contains(r#""exit_code":0"#));
    }

    #[test]
    fn agent_message_job_completed_failure() {
        let msg = AgentMessage::JobCompleted { exit_code: 1 };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""exit_code":1"#));
    }

    #[test]
    fn agent_messages_are_ndjson_compatible() {
        // Each message should be a single line of JSON
        let messages = vec![
            AgentMessage::Ready { timestamp: "t".to_string() },
            AgentMessage::JobStarted { job_id: Some(1) },
            AgentMessage::Log { line: "test".to_string() },
            AgentMessage::JobCompleted { exit_code: 0 },
        ];
        for msg in messages {
            let json = serde_json::to_string(&msg).unwrap();
            assert!(!json.contains('\n'), "NDJSON messages must not contain newlines");
            // Verify it's valid JSON
            let _: serde_json::Value = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn mmds_base_url() {
        assert_eq!(MMDS_BASE, "http://169.254.169.254");
    }

    #[test]
    fn runner_dir_path() {
        assert_eq!(RUNNER_DIR, "/home/runner/actions-runner");
    }

    #[tokio::test]
    async fn mmds_read_failure_with_retry() {
        // Attempting to read MMDS from a non-existent server should fail
        let result = read_mmds_with_retry(2, std::time::Duration::from_millis(10)).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("failed to read MMDS"));
    }
}
