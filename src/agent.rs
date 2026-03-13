//! Guest agent — runs inside a Firecracker VM.
//!
//! Reads MMDS metadata, sets hostname, starts the GitHub Actions runner,
//! and reports state to the host via VSOCK (NDJSON on port 1024).
//!
//! The runner process gets an explicit, stripped environment (like fireactions):
//! only PATH, HOME, USER, LOGNAME are set. This ensures tools like cargo
//! are available if installed to standard PATH directories.

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
/// Path to the actions runner directory inside the VM.
const RUNNER_DIR: &str = "/home/runner";
/// Runner user name.
const RUNNER_USER: &str = "runner";
/// Explicit PATH for the runner process — includes cargo and standard dirs.
const RUNNER_PATH: &str = "/home/runner/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Metadata expected from MMDS.
#[derive(Debug, Deserialize)]
struct Metadata {
    runner_jit_config: Option<String>,
    #[serde(default)]
    runner_token: Option<String>,
    #[serde(default)]
    runner_mode: Option<String>,
    #[serde(default)]
    repo_url: Option<String>,
    #[serde(default)]
    runner_name: Option<String>,
    hostname: String,
    #[serde(default)]
    shutdown_on_exit: bool,
    #[serde(default = "default_ephemeral")]
    ephemeral: bool,
}

fn default_ephemeral() -> bool {
    true
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

    // Wait for Docker daemon to be ready before starting the runner.
    // GitHub Actions services: containers need Docker, and the daemon may still
    // be starting when we reach this point (race with systemd boot).
    wait_for_docker(30, Duration::from_secs(2)).await;

    let exit_code = run_with_reporting(&metadata).await;

    if metadata.shutdown_on_exit {
        tracing::info!("shutting down VM");
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Use reboot -f (not poweroff -f): Firecracker has no ACPI, so poweroff
        // halts the CPU in a loop. reboot -f with reboot=k boot arg triggers
        // keyboard controller reset → KVM_EXIT_SHUTDOWN → clean VMM exit.
        let _ = tokio::process::Command::new("reboot")
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

    let exit_code = run_runner(metadata).await;

    tracing::info!(exit_code, "runner exited");
    vsock_send(&mut stream, &AgentMessage::JobCompleted { exit_code }).await;

    if let Some(s) = &mut stream {
        let _ = s.flush().await;
    }

    exit_code
}

#[cfg(not(target_os = "linux"))]
async fn run_with_reporting(metadata: &Metadata) -> i32 {
    tracing::warn!("VSOCK not available on this platform, running without host reporting");
    tracing::info!("starting GitHub Actions runner");
    let exit_code = run_runner(metadata).await;
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

/// Set the system hostname and update /etc/hosts.
async fn set_hostname(hostname: &str) {
    if let Err(e) = tokio::process::Command::new("hostname")
        .arg(hostname)
        .status()
        .await
    {
        tracing::warn!(error = %e, "failed to set hostname");
    }
    let hosts = format!(
        "127.0.0.1 localhost localhost.localdomain {hostname}\n\
         ::1 localhost ip6-localhost ip6-loopback {hostname}\n"
    );
    if let Err(e) = tokio::fs::write("/etc/hosts", hosts).await {
        tracing::warn!(error = %e, "failed to update /etc/hosts");
    }
}

/// Wait for Docker daemon socket to appear and become accessible.
/// Logs detailed diagnostics if Docker is not ready after all attempts.
async fn wait_for_docker(max_attempts: u32, delay: Duration) {
    let socket_path = "/var/run/docker.sock";

    for attempt in 1..=max_attempts {
        // First check if the socket file exists
        if tokio::fs::metadata(socket_path).await.is_err() {
            if attempt == max_attempts {
                tracing::warn!(
                    "Docker socket {socket_path} not found after {max_attempts} attempts — \
                     services: containers will fail"
                );
                log_docker_diagnostics().await;
                return;
            }
            tracing::info!(attempt, "waiting for Docker socket to appear...");
            tokio::time::sleep(delay).await;
            continue;
        }

        // Socket exists — try `docker version` as the runner user to verify access
        let mut cmd = tokio::process::Command::new("/usr/bin/docker");
        cmd.args(["version", "--format", "{{.Server.APIVersion}}"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        as_runner_user(&mut cmd);

        match cmd.output().await {
            Ok(output) if output.status.success() => {
                let api_version = String::from_utf8_lossy(&output.stdout);
                tracing::info!(
                    api_version = api_version.trim(),
                    attempt,
                    "Docker daemon is ready"
                );
                return;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if attempt == max_attempts {
                    tracing::warn!(
                        stderr = stderr.trim(),
                        "Docker not accessible after {max_attempts} attempts"
                    );
                    log_docker_diagnostics().await;
                    return;
                }
                tracing::info!(attempt, stderr = stderr.trim(), "Docker not ready yet, retrying...");
            }
            Err(e) => {
                if attempt == max_attempts {
                    tracing::warn!(error = %e, "docker command failed after {max_attempts} attempts");
                    return;
                }
                tracing::info!(attempt, error = %e, "docker command failed, retrying...");
            }
        }
        tokio::time::sleep(delay).await;
    }
}

/// Log diagnostic info about Docker to help troubleshoot permission issues.
async fn log_docker_diagnostics() {
    // Check socket permissions
    let socket_path = "/var/run/docker.sock";
    if let Ok(output) = tokio::process::Command::new("ls")
        .args(["-la", socket_path])
        .output()
        .await
    {
        let info = String::from_utf8_lossy(&output.stdout);
        tracing::warn!(socket = info.trim(), "Docker socket permissions");
    }

    // Check if docker daemon is running
    if let Ok(output) = tokio::process::Command::new("pgrep")
        .args(["-a", "dockerd"])
        .output()
        .await
    {
        let info = String::from_utf8_lossy(&output.stdout);
        if info.trim().is_empty() {
            tracing::warn!("dockerd process NOT running");
        } else {
            tracing::warn!(process = info.trim(), "dockerd process found");
        }
    }

    // Show runner user's groups
    let (uid, gid, groups) = runner_credentials();
    tracing::warn!(uid, gid, ?groups, "runner user credentials");

    // Check docker group GID
    if let Ok(contents) = tokio::fs::read_to_string("/etc/group").await {
        for line in contents.lines() {
            if line.starts_with("docker:") {
                tracing::warn!(group_entry = line, "docker group in /etc/group");
                break;
            }
        }
    }

    // Check systemd service status
    if let Ok(output) = tokio::process::Command::new("systemctl")
        .args(["status", "docker", "--no-pager", "-l"])
        .output()
        .await
    {
        let info = String::from_utf8_lossy(&output.stdout);
        let err = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(status = info.trim(), stderr = err.trim(), "docker.service status");
    }
}

/// Build the explicit environment for the runner process.
/// Starts empty (like fireactions) and only sets known-good variables.
fn runner_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("PATH", RUNNER_PATH),
        ("HOME", "/home/runner"),
        ("USER", RUNNER_USER),
        ("LOGNAME", RUNNER_USER),
    ]
}

/// Look up the UID, primary GID, and supplementary group GIDs for the runner user.
/// Falls back to 1000:1000 with no supplementary groups if the user doesn't exist.
fn runner_credentials() -> (u32, u32, Vec<u32>) {
    let mut uid = 1000u32;
    let mut primary_gid = 1000u32;

    // Read /etc/passwd to find the runner user's UID/GID
    if let Ok(contents) = std::fs::read_to_string("/etc/passwd") {
        for line in contents.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() >= 4 && fields[0] == RUNNER_USER {
                uid = fields[2].parse().unwrap_or(1000);
                primary_gid = fields[3].parse().unwrap_or(1000);
                break;
            }
        }
    }

    // Read /etc/group to find supplementary groups (e.g., docker)
    let mut groups = vec![primary_gid];
    if let Ok(contents) = std::fs::read_to_string("/etc/group") {
        for line in contents.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() >= 4 {
                let gid: u32 = fields[2].parse().unwrap_or(0);
                if gid == primary_gid {
                    continue;
                }
                // Check if runner is in this group's member list
                let members: Vec<&str> = fields[3].split(',').collect();
                if members.iter().any(|m| m.trim() == RUNNER_USER) {
                    groups.push(gid);
                }
            }
        }
    }

    (uid, primary_gid, groups)
}

/// Apply runner user credentials to a command (drop privileges from root).
/// Sets UID, GID, and supplementary groups (e.g., docker) so the runner
/// can access resources like /var/run/docker.sock.
#[cfg(unix)]
fn as_runner_user(cmd: &mut tokio::process::Command) {
    let (uid, gid, groups) = runner_credentials();
    // SAFETY: setgroups/setgid/setuid are async-signal-safe per POSIX.
    // Must call setgroups before setgid/setuid (can't set groups after dropping root).
    unsafe {
        cmd.pre_exec(move || {
            let c_groups: Vec<u32> = groups.clone();
            if !c_groups.is_empty() {
                let ret = libc::setgroups(
                    c_groups.len() as libc::c_int,
                    c_groups.as_ptr() as *const libc::gid_t,
                );
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if libc::setgid(gid as libc::gid_t) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setuid(uid as libc::uid_t) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Run the GitHub Actions runner and return its exit code.
///
/// Supports both JIT mode (runner_jit_config) and registration mode (runner_token).
/// The runner process gets an explicit, stripped environment with cargo in PATH.
async fn run_runner(metadata: &Metadata) -> i32 {
    let mode = metadata.runner_mode.as_deref().unwrap_or("jit");

    if mode == "register" || mode == "registration" {
        run_runner_registered(metadata).await
    } else {
        run_runner_jit(metadata).await
    }
}

/// Run the runner in JIT mode (--jitconfig).
async fn run_runner_jit(metadata: &Metadata) -> i32 {
    let jit_config = match &metadata.runner_jit_config {
        Some(c) => c,
        None => {
            tracing::error!("no runner_jit_config in MMDS metadata");
            return 1;
        }
    };

    let run_sh = format!("{}/run.sh", RUNNER_DIR);
    tracing::info!("starting runner (JIT mode)");

    let mut cmd = tokio::process::Command::new(&run_sh);
    cmd.arg("--jitconfig")
        .arg(jit_config)
        .current_dir(RUNNER_DIR)
        .env_clear()
        .envs(runner_env())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    as_runner_user(&mut cmd);
    let mut child = match cmd.spawn()
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

/// Run the runner in registration mode (config.sh + run.sh).
async fn run_runner_registered(metadata: &Metadata) -> i32 {
    let token = match &metadata.runner_token {
        Some(t) => t,
        None => {
            tracing::error!("no runner_token in MMDS metadata for registration mode");
            return 1;
        }
    };
    let repo_url = match &metadata.repo_url {
        Some(u) => u,
        None => {
            tracing::error!("no repo_url in MMDS metadata for registration mode");
            return 1;
        }
    };
    let runner_name = metadata.runner_name.as_deref()
        .unwrap_or(&metadata.hostname);
    let labels = "firecracker,linux,x64";

    let config_sh = format!("{}/config.sh", RUNNER_DIR);
    tracing::info!(runner_name, repo_url = %repo_url, "registering runner");

    let work_dir = format!("{}/_work", RUNNER_DIR);
    let mut cmd = tokio::process::Command::new(&config_sh);
    let mut args = vec![
        "--url", repo_url,
        "--token", token,
        "--name", runner_name,
        "--labels", labels,
        "--unattended",
        "--disableupdate",
        "--work", &work_dir,
    ];
    if metadata.ephemeral {
        args.push("--ephemeral");
    }
    cmd.args(&args)
        .current_dir(RUNNER_DIR)
        .env_clear()
        .envs(runner_env())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    as_runner_user(&mut cmd);
    let status = match cmd.status().await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to start config.sh");
            return 1;
        }
    };

    if !status.success() {
        tracing::error!(code = status.code(), "config.sh failed");
        return status.code().unwrap_or(1);
    }

    let run_sh = format!("{}/run.sh", RUNNER_DIR);
    tracing::info!("starting runner (registered mode)");

    let mut cmd = tokio::process::Command::new(&run_sh);
    cmd.current_dir(RUNNER_DIR)
        .env_clear()
        .envs(runner_env())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    as_runner_user(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to start run.sh");
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
    fn metadata_deserialization_jit() {
        let json = r#"{"runner_jit_config":"abc123","hostname":"fc-42","shutdown_on_exit":true}"#;
        let meta: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.runner_jit_config.as_deref(), Some("abc123"));
        assert_eq!(meta.hostname, "fc-42");
        assert!(meta.shutdown_on_exit);
    }

    #[test]
    fn metadata_deserialization_register() {
        let json = r#"{"runner_token":"tok","hostname":"fc-warm-0","repo_url":"https://github.com/org/repo","runner_name":"fc-warm-0-abc","runner_mode":"register"}"#;
        let meta: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.runner_token.as_deref(), Some("tok"));
        assert_eq!(meta.runner_mode.as_deref(), Some("register"));
        assert_eq!(meta.repo_url.as_deref(), Some("https://github.com/org/repo"));
        assert_eq!(meta.runner_name.as_deref(), Some("fc-warm-0-abc"));
        assert!(!meta.shutdown_on_exit);
    }

    #[test]
    fn metadata_shutdown_defaults_false() {
        let json = r#"{"runner_jit_config":"token","hostname":"fc-0"}"#;
        let meta: Metadata = serde_json::from_str(json).unwrap();
        assert!(!meta.shutdown_on_exit);
    }

    #[test]
    fn metadata_minimal_fields() {
        // Only hostname is truly required
        let json = r#"{"hostname":"fc-0"}"#;
        let meta: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.hostname, "fc-0");
        assert!(meta.runner_jit_config.is_none());
        assert!(meta.runner_token.is_none());
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
        let messages = vec![
            AgentMessage::Ready { timestamp: "t".to_string() },
            AgentMessage::JobStarted { job_id: Some(1) },
            AgentMessage::Log { line: "test".to_string() },
            AgentMessage::JobCompleted { exit_code: 0 },
        ];
        for msg in messages {
            let json = serde_json::to_string(&msg).unwrap();
            assert!(!json.contains('\n'), "NDJSON messages must not contain newlines");
            let _: serde_json::Value = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn mmds_base_url() {
        assert_eq!(MMDS_BASE, "http://169.254.169.254");
    }

    #[test]
    fn runner_dir_path() {
        assert_eq!(RUNNER_DIR, "/home/runner");
    }

    #[test]
    fn runner_path_includes_cargo() {
        assert!(RUNNER_PATH.contains("/home/runner/.cargo/bin"));
    }

    #[test]
    fn runner_env_has_required_vars() {
        let env = runner_env();
        let keys: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"PATH"));
        assert!(keys.contains(&"HOME"));
        assert!(keys.contains(&"USER"));
        assert!(keys.contains(&"LOGNAME"));
    }

    #[tokio::test]
    async fn mmds_read_failure_with_retry() {
        let result = read_mmds_with_retry(2, std::time::Duration::from_millis(10)).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("failed to read MMDS"));
    }
}
