/// Host-side VSOCK listener for guest agent communication.
///
/// Protocol: NDJSON over VSOCK port 1024.
/// The guest agent sends messages, the host logs them and updates state.
/// When a `JobCompleted` message arrives, the optional `notify_tx` channel
/// is used to signal the orchestrator to begin spawning a replacement VM
/// before the current VM fully shuts down.
use serde::Deserialize;
use tokio::sync::mpsc;

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
/// If `notify_tx` is provided, a `JobDoneNotification` is sent when the guest
/// agent reports `JobCompleted`. This allows the orchestrator to begin
/// spinning up a replacement VM before the current one fully shuts down.
#[cfg(target_os = "linux")]
pub fn spawn_listener(
    vm_id: String,
    cid: u32,
    notify_tx: Option<mpsc::Sender<JobDoneNotification>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = listen_loop(&vm_id, cid, notify_tx).await {
            tracing::warn!(vm_id = %vm_id, cid, error = %e, "VSOCK listener ended");
        }
    })
}

#[cfg(target_os = "linux")]
async fn listen_loop(
    vm_id: &str,
    cid: u32,
    notify_tx: Option<mpsc::Sender<JobDoneNotification>>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio_vsock::VsockListener;

    let addr = tokio_vsock::VsockAddr::new(cid, AGENT_PORT);
    let mut listener = VsockListener::bind(addr)
        .map_err(|e| anyhow::anyhow!("VSOCK bind CID {} port {}: {}", cid, AGENT_PORT, e))?;

    tracing::info!(vm_id = %vm_id, cid, port = AGENT_PORT, "VSOCK listener started");

    // Accept one connection (the guest agent)
    let (stream, _addr) = listener
        .accept()
        .await
        .map_err(|e| anyhow::anyhow!("VSOCK accept: {}", e))?;

    tracing::info!(vm_id = %vm_id, cid, "guest agent connected via VSOCK");

    let reader = BufReader::new(stream);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<AgentMessage>(&line) {
            Ok(msg) => {
                handle_message(vm_id, &msg);

                // Notify orchestrator on job completion
                if let AgentMessage::JobCompleted { exit_code } = &msg {
                    if let Some(tx) = &notify_tx {
                        let _ = tx
                            .send(JobDoneNotification {
                                vm_id: vm_id.to_string(),
                                exit_code: *exit_code,
                            })
                            .await;
                    }
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

    tracing::info!(vm_id = %vm_id, cid, "guest agent disconnected");
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

/// Stub for non-Linux platforms.
#[cfg(not(target_os = "linux"))]
pub fn spawn_listener(
    vm_id: String,
    _cid: u32,
    _notify_tx: Option<mpsc::Sender<JobDoneNotification>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::warn!(vm_id = %vm_id, "VSOCK not supported on this platform");
    })
}
