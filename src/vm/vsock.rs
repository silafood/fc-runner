/// Host-side VSOCK listener for guest agent communication.
///
/// Protocol: NDJSON over VSOCK port 1024.
/// The guest agent sends messages, the host logs them and updates state.
/// When a `JobCompleted` message arrives, the optional `notify_tx` channel
/// is used to signal the orchestrator to begin spawning a replacement VM
/// before the current VM fully shuts down.
///
/// All agent messages are also published to the `log_tx` broadcast channel
/// for SSE streaming via the management API.
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};

use crate::api::VmLogEvent;

/// Guest agent message types (NDJSON protocol).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum AgentMessage {
    Ready {
        #[serde(default)]
        timestamp: Option<String>,
    },
    JobStarted {
        #[serde(default)]
        job_id: Option<u64>,
    },
    Log {
        line: String,
    },
    JobCompleted {
        exit_code: i32,
    },
    Heartbeat,
}

/// VSOCK port the guest agent connects to.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const AGENT_PORT: u32 = 1024;

/// Notification sent when the guest agent reports job completion.
#[derive(Debug)]
pub struct JobDoneNotification {
    pub vm_id: String,
    pub exit_code: i32,
}

/// Spawn a VSOCK listener for a given VM.
/// Returns a JoinHandle that reads messages until the guest disconnects.
///
/// Firecracker proxies guest VSOCK connections to Unix domain sockets on the host.
/// When a guest connects to host CID 2 on port P, Firecracker forwards it to
/// `{uds_path}_{P}` as a Unix stream connection. We listen on that UDS path.
///
/// If `notify_tx` is provided, a `JobDoneNotification` is sent when the guest
/// agent reports `JobCompleted`. This allows the orchestrator to begin
/// spinning up a replacement VM before the current one fully shuts down.
///
/// If `log_tx` is provided, all agent messages are published as `VmLogEvent`s
/// for SSE streaming.
#[cfg(target_os = "linux")]
pub fn spawn_listener(
    vm_id: String,
    uds_path: std::path::PathBuf,
    notify_tx: Option<mpsc::Sender<JobDoneNotification>>,
    log_tx: Option<broadcast::Sender<VmLogEvent>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = listen_loop(&vm_id, &uds_path, notify_tx, log_tx).await {
            tracing::warn!(vm_id = %vm_id, uds = %uds_path.display(), error = %e, "VSOCK listener ended");
        }
    })
}

#[cfg(target_os = "linux")]
async fn listen_loop(
    vm_id: &str,
    uds_path: &std::path::Path,
    notify_tx: Option<mpsc::Sender<JobDoneNotification>>,
    log_tx: Option<broadcast::Sender<VmLogEvent>>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Firecracker creates {uds_path}_{port} when a guest connects to that port.
    // We must listen on that path before the guest connects.
    let listen_path = format!("{}_{}", uds_path.display(), AGENT_PORT);

    // Remove stale socket from previous runs
    let _ = tokio::fs::remove_file(&listen_path).await;

    let listener = tokio::net::UnixListener::bind(&listen_path)
        .map_err(|e| anyhow::anyhow!("VSOCK UDS bind {}: {}", listen_path, e))?;

    tracing::info!(vm_id = %vm_id, uds = %listen_path, "VSOCK listener started (Unix socket)");

    // Accept one connection (the guest agent via Firecracker's VSOCK proxy)
    let (stream, _addr) = listener
        .accept()
        .await
        .map_err(|e| anyhow::anyhow!("VSOCK UDS accept: {}", e))?;

    tracing::info!(vm_id = %vm_id, "guest agent connected via VSOCK");

    let reader = BufReader::new(stream);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<AgentMessage>(&line) {
            Ok(msg) => {
                handle_message(vm_id, &msg);

                // Publish to SSE broadcast channel
                if let Some(tx) = &log_tx
                    && let Some(event) = message_to_log_event(vm_id, &msg)
                {
                    let _ = tx.send(event);
                }

                // Notify orchestrator on job completion
                if let AgentMessage::JobCompleted { exit_code } = &msg
                    && let Some(tx) = &notify_tx
                {
                    let _ = tx
                        .send(JobDoneNotification {
                            vm_id: vm_id.to_string(),
                            exit_code: *exit_code,
                        })
                        .await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    vm_id = %vm_id,
                    error = %e,
                    line = %line,
                    "invalid agent message"
                );
            }
        }
    }

    // Clean up the listener socket
    let _ = tokio::fs::remove_file(&listen_path).await;

    tracing::info!(vm_id = %vm_id, "guest agent disconnected");
    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn handle_message(vm_id: &str, msg: &AgentMessage) {
    match msg {
        AgentMessage::Ready { timestamp } => {
            tracing::info!(
                vm_id = %vm_id,
                timestamp = ?timestamp,
                "guest agent ready"
            );
        }
        AgentMessage::JobStarted { job_id } => {
            tracing::info!(
                vm_id = %vm_id,
                job_id = ?job_id,
                "guest job started"
            );
        }
        AgentMessage::Log { line } => {
            tracing::info!(vm_id = %vm_id, "[agent] {}", line);
        }
        AgentMessage::JobCompleted { exit_code } => {
            tracing::info!(
                vm_id = %vm_id,
                exit_code,
                "guest job completed"
            );
        }
        AgentMessage::Heartbeat => {
            tracing::trace!(vm_id = %vm_id, "heartbeat");
        }
    }
}

/// Convert an agent message into a VmLogEvent for SSE streaming.
/// Returns None for Heartbeat (too noisy for SSE).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn message_to_log_event(vm_id: &str, msg: &AgentMessage) -> Option<VmLogEvent> {
    let now = chrono::Utc::now().to_rfc3339();
    let (event_type, message) = match msg {
        AgentMessage::Ready { timestamp } => (
            "ready",
            format!(
                "guest agent ready{}",
                timestamp
                    .as_deref()
                    .map(|t| format!(" (at {})", t))
                    .unwrap_or_default()
            ),
        ),
        AgentMessage::JobStarted { job_id } => (
            "job_started",
            format!(
                "job started{}",
                job_id
                    .map(|id| format!(" (job_id: {})", id))
                    .unwrap_or_default()
            ),
        ),
        AgentMessage::Log { line } => ("log", line.clone()),
        AgentMessage::JobCompleted { exit_code } => (
            "job_completed",
            format!("job completed (exit_code: {})", exit_code),
        ),
        AgentMessage::Heartbeat => return None,
    };

    Some(VmLogEvent {
        vm_id: vm_id.to_string(),
        event_type: event_type.to_string(),
        message,
        timestamp: now,
    })
}

/// Stub for non-Linux platforms.
#[cfg(not(target_os = "linux"))]
pub fn spawn_listener(
    vm_id: String,
    _uds_path: std::path::PathBuf,
    _notify_tx: Option<mpsc::Sender<JobDoneNotification>>,
    _log_tx: Option<broadcast::Sender<VmLogEvent>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::warn!(vm_id = %vm_id, "VSOCK not supported on this platform");
    })
}
