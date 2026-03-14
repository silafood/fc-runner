use anyhow::Context;
use firecracker_rs_sdk::models::MmdsContentsObject;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::MicroVm;

/// Send MMDS metadata directly via raw HTTP on the Firecracker API socket.
///
/// The SDK's `put_mmds()` double-serializes: `MmdsContentsObject` is a `String`,
/// and `serde_json::to_vec(&string)` wraps it in JSON quotes, so Firecracker
/// receives `"{\"fc-runner\":...}"` instead of `{"fc-runner":...}`.
pub(crate) async fn put_mmds_raw(
    socket_path: &std::path::Path,
    json_body: &str,
) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to API socket: {}", socket_path.display()))?;

    let request = format!(
        "PUT /mmds HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        json_body.len(),
        json_body
    );
    stream
        .write_all(request.as_bytes())
        .await
        .context("writing MMDS PUT request")?;

    let mut response = vec![0u8; 1024];
    let n = stream
        .read(&mut response)
        .await
        .context("reading MMDS PUT response")?;
    let response_str = String::from_utf8_lossy(&response[..n]);

    if !response_str.contains("204") && !response_str.contains("200") {
        anyhow::bail!(
            "MMDS PUT failed: {}",
            response_str.lines().next().unwrap_or("")
        );
    }

    Ok(())
}

/// Configure VSOCK via raw HTTP API, bypassing the SDK which incorrectly
/// deserializes Firecracker's `{}` response as a unit struct.
pub(crate) async fn put_vsock_raw(
    socket_path: &std::path::Path,
    json_body: &str,
) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to API socket: {}", socket_path.display()))?;

    let request = format!(
        "PUT /vsock HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        json_body.len(),
        json_body
    );
    stream
        .write_all(request.as_bytes())
        .await
        .context("writing VSOCK PUT request")?;

    let mut response = vec![0u8; 1024];
    let n = stream
        .read(&mut response)
        .await
        .context("reading VSOCK PUT response")?;
    let response_str = String::from_utf8_lossy(&response[..n]);

    if !response_str.contains("204") && !response_str.contains("200") {
        anyhow::bail!("VSOCK PUT failed: {}", response_str.trim());
    }

    Ok(())
}

impl MicroVm {
    /// Build the MMDS metadata JSON string from env_content (KEY=VALUE lines).
    pub(crate) fn build_mmds_payload(
        &self,
        env_content: &str,
    ) -> anyhow::Result<MmdsContentsObject> {
        let mut inner = serde_json::Map::new();
        for line in env_content.lines() {
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim().to_lowercase();
                let value = value.trim();
                if value == "true" || value == "false" {
                    inner.insert(key, serde_json::Value::Bool(value == "true"));
                } else {
                    inner.insert(key, serde_json::Value::String(value.to_string()));
                }
            }
        }
        let mut metadata = serde_json::Map::new();
        metadata.insert("fc-runner".to_string(), serde_json::Value::Object(inner));
        let json = serde_json::to_string(&serde_json::Value::Object(metadata))
            .context("serializing MMDS metadata")?;
        Ok(json)
    }
}
