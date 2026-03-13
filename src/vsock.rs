/// Host-side VSOCK listener for guest agent communication.
///
/// Protocol: NDJSON over VSOCK port 1024.
/// The guest agent sends messages, the host logs them and updates state.
use serde::Deserialize;

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

/// Spawn a VSOCK listener for a given VM.
/// Returns a JoinHandle that reads messages until the guest disconnects.
#[cfg(target_os = "linux")]
pub fn spawn_listener(vm_id: String, cid: u32) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = listen_loop(&vm_id, cid).await {
            tracing::warn!(vm_id = %vm_id, cid, error = %e, "VSOCK listener ended");
        }
    })
}

#[cfg(target_os = "linux")]
async fn listen_loop(vm_id: &str, cid: u32) -> anyhow::Result<()> {
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
            Ok(msg) => handle_message(vm_id, &msg),
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
pub fn spawn_listener(vm_id: String, _cid: u32) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::warn!(vm_id = %vm_id, "VSOCK not supported on this platform");
    })
}
