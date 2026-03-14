use std::path::PathBuf;

use anyhow::{Context, ensure};
use firecracker_rs_sdk::firecracker::FirecrackerOption;
use firecracker_rs_sdk::instance::Instance;
use firecracker_rs_sdk::jailer::JailerOption;
use firecracker_rs_sdk::models::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{FirecrackerConfig, NetworkConfig};
use crate::netlink;
use crate::vsock;

/// Mount an ext4 image via loop (read-write).
///
/// Loop device setup is handled by the `mount` command (userspace), not the
/// kernel mount(2) syscall, so we must use Command here.
fn mount_loop_ext4(image: &str, target: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("mount")
        .args(["-o", "loop", image, target])
        .status()
        .context("running mount")?;
    ensure!(
        status.success(),
        "mount -o loop {} {} failed",
        image,
        target
    );
    Ok(())
}

/// Mount an ext4 image via loop (read-only, noload for dirty fs).
fn mount_loop_ext4_ro(image: &str, target: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("mount")
        .args(["-o", "loop,ro,noload", image, target])
        .status()
        .context("running mount (ro)")?;
    ensure!(
        status.success(),
        "mount -o loop,ro {} {} failed",
        image,
        target
    );
    Ok(())
}

/// Try a normal umount, returns true on success.
fn try_umount(target: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        nix::mount::umount(target).is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("umount")
            .arg(target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Lazy (detach) umount.
fn lazy_umount_sync(target: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        nix::mount::umount2(target, nix::mount::MntFlags::MNT_DETACH)
            .map_err(|e| anyhow::anyhow!("lazy umount failed: {}", e))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let status = std::process::Command::new("umount")
            .args(["-l", target])
            .status()?;
        ensure!(status.success(), "lazy umount failed");
        Ok(())
    }
}

/// Check if a process is still alive by sending signal 0.
fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // Try waitpid(WNOHANG) first — this reaps zombies and detects exit.
        // kill(pid, 0) is insufficient because it returns Ok for zombies.
        match nix::sys::wait::waitpid(
            nix::unistd::Pid::from_raw(pid as i32),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        ) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) => true,
            Ok(_) => false, // exited, signaled, or stopped — not alive
            Err(nix::errno::Errno::ECHILD) => {
                // Not our child (or already reaped). Fall back to kill(0)
                // but also check /proc/{pid}/status for zombie state.
                if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err() {
                    return false;
                }
                // Process exists but isn't our child — check for zombie
                std::fs::read_to_string(format!("/proc/{}/status", pid))
                    .map(|s| !s.contains("\nState:\tZ"))
                    .unwrap_or(false)
            }
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux (e.g. macOS for dev), check if /proc/{pid} exists
        // or just assume alive (Firecracker only runs on Linux anyway)
        std::path::Path::new(&format!("/proc/{}", pid)).exists()
    }
}

/// Kill a process by PID (SIGKILL for immediate termination).
fn kill_process(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

/// Send MMDS metadata directly via raw HTTP on the Firecracker API socket.
///
/// The SDK's `put_mmds()` double-serializes: `MmdsContentsObject` is a `String`,
/// and `serde_json::to_vec(&string)` wraps it in JSON quotes, so Firecracker
/// receives `"{\"fc-runner\":...}"` instead of `{"fc-runner":...}`.
async fn put_mmds_raw(socket_path: &std::path::Path, json_body: &str) -> anyhow::Result<()> {
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
async fn put_vsock_raw(socket_path: &std::path::Path, json_body: &str) -> anyhow::Result<()> {
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

pub struct MicroVm {
    pub vm_id: String,
    pub job_id: u64,
    pub slot: usize,
    rootfs_path: PathBuf,
    config_path: PathBuf,
    socket_path: PathBuf,
    log_path: PathBuf,
    mount_point: PathBuf,
    /// Per-VM overlay ext4 file (used only when overlay_rootfs = true).
    overlay_path: PathBuf,
    /// Per-VM VSOCK Unix socket path (used only when vsock_enabled = true).
    vsock_socket_path: PathBuf,
    /// Per-slot persistent cache ext4 image (used only when cache_enabled = true).
    cache_path: Option<PathBuf>,
    /// Token for the Actions cache service (set when cache_service is enabled).
    pub cache_service_token: Option<String>,
    /// Port the cache service listens on (from server.listen_addr).
    pub cache_service_port: Option<u16>,
    fc_config: FirecrackerConfig,
    vm_timeout_secs: u64,
    cancel: CancellationToken,
    // Per-VM networking
    tap_name: String,
    host_ip: String,
    guest_ip: String,
    guest_mac: String,
    network_dns: Vec<String>,
}

/// Convert a PathBuf to &str with a descriptive error instead of panicking.
fn path_str(path: &std::path::Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path contains invalid UTF-8: {}", path.display()))
}

/// Extract the filename from a path as a &str, with a descriptive error.
fn filename_str(path: &std::path::Path) -> anyhow::Result<&str> {
    path.file_name()
        .ok_or_else(|| anyhow::anyhow!("path has no filename: {}", path.display()))?
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("filename contains invalid UTF-8: {}", path.display()))
}

impl MicroVm {
    pub fn new(
        job_id: u64,
        fc_config: &FirecrackerConfig,
        network: &NetworkConfig,
        work_dir: &str,
        vm_timeout_secs: u64,
        slot: usize,
        cancel: CancellationToken,
    ) -> Self {
        let vm_id = format!("fc-{}-{}", job_id, Uuid::new_v4().simple());
        let base = PathBuf::from(work_dir);
        let cache_path = if fc_config.cache_enabled {
            Some(PathBuf::from(&fc_config.cache_dir).join(format!("slot-{}.ext4", slot)))
        } else {
            None
        };
        Self {
            rootfs_path: base.join(format!("{}.ext4", vm_id)),
            config_path: base.join(format!("{}.json", vm_id)),
            socket_path: base.join(format!("{}.sock", vm_id)),
            log_path: base.join(format!("{}.log", vm_id)),
            mount_point: base.join(format!("{}.mnt", vm_id)),
            overlay_path: base.join(format!("{}.overlay.ext4", vm_id)),
            vsock_socket_path: base.join(format!("{}.vsock", vm_id)),
            cache_path,
            cache_service_token: None,
            cache_service_port: None,
            job_id,
            vm_id,
            fc_config: fc_config.clone(),
            vm_timeout_secs,
            cancel,
            slot,
            tap_name: format!("tap-fc{}", slot),
            host_ip: format!("172.16.{}.1", slot),
            guest_ip: format!("172.16.{}.2", slot),
            guest_mac: format!("06:00:AC:10:{:02X}:02", slot),
            network_dns: network.dns.clone(),
        }
    }

    fn use_mmds(&self) -> bool {
        self.fc_config.secret_injection == "mmds"
    }

    fn use_overlay(&self) -> bool {
        self.fc_config.overlay_rootfs
    }

    /// Path to the shared squashfs rootfs (derived from golden rootfs path).
    fn squashfs_path(&self) -> String {
        self.fc_config.rootfs_golden.replace(".ext4", ".squashfs")
    }

    /// Guest device name for the cache volume.
    /// In overlay mode: vda=squashfs, vdb=overlay, vdc=cache
    /// In legacy mode: vda=rootfs, vdb=cache
    fn cache_device_name(&self) -> &str {
        if self.use_overlay() { "vdc" } else { "vdb" }
    }

    /// Create a per-VM sparse overlay ext4 file for OverlayFS COW mode.
    async fn create_overlay(&self) -> anyhow::Result<()> {
        let size_bytes = self.fc_config.overlay_size_mib as u64 * 1024 * 1024;
        let overlay = path_str(&self.overlay_path)?;

        tracing::info!(
            vm_id = %self.vm_id,
            size_mib = self.fc_config.overlay_size_mib,
            "creating sparse overlay ext4"
        );

        // Create sparse file (instant, no actual disk allocation)
        let f = std::fs::File::create(overlay).context("creating overlay file")?;
        f.set_len(size_bytes).context("setting overlay size")?;
        drop(f);

        // Format as ext4
        let status = Command::new("mkfs.ext4")
            .args(["-F", "-q", overlay])
            .status()
            .await
            .context("running mkfs.ext4 on overlay")?;
        ensure!(status.success(), "mkfs.ext4 overlay failed");
        Ok(())
    }

    /// Ensure the per-slot persistent cache ext4 image exists at the configured size.
    /// Creates a sparse file + formats it if missing. Recreates the image if the
    /// configured size differs from the existing file (e.g. after a config change).
    /// This is NOT per-VM — it persists across VM lifecycles for the same slot.
    async fn ensure_cache_image(&self) -> anyhow::Result<()> {
        let cache_path = match &self.cache_path {
            Some(p) => p,
            None => return Ok(()),
        };
        let size_bytes = self.fc_config.cache_size_mib as u64 * 1024 * 1024;
        if cache_path.exists() {
            let meta = tokio::fs::metadata(cache_path).await?;
            if meta.len() == size_bytes {
                tracing::debug!(
                    vm_id = %self.vm_id,
                    slot = self.slot,
                    cache = %cache_path.display(),
                    "cache image already exists"
                );
                return Ok(());
            }
            tracing::warn!(
                vm_id = %self.vm_id,
                slot = self.slot,
                current_mib = meta.len() / (1024 * 1024),
                configured_mib = self.fc_config.cache_size_mib,
                "cache image size mismatch, recreating"
            );
            tokio::fs::remove_file(cache_path).await?;
        }
        let cache_dir = cache_path.parent().unwrap();
        tokio::fs::create_dir_all(cache_dir)
            .await
            .with_context(|| format!("creating cache directory: {}", cache_dir.display()))?;

        let cache_str = path_str(cache_path)?;

        tracing::info!(
            vm_id = %self.vm_id,
            slot = self.slot,
            size_mib = self.fc_config.cache_size_mib,
            path = cache_str,
            "creating persistent cache image (sparse ext4)"
        );

        let f = std::fs::File::create(cache_str).context("creating cache file")?;
        f.set_len(size_bytes).context("setting cache file size")?;
        drop(f);

        let status = Command::new("mkfs.ext4")
            .args(["-F", "-q", "-L", "fc-cache", cache_str])
            .status()
            .await
            .context("running mkfs.ext4 on cache image")?;
        ensure!(status.success(), "mkfs.ext4 cache image failed");
        Ok(())
    }

    async fn copy_rootfs(&self) -> anyhow::Result<()> {
        tracing::info!(vm_id = %self.vm_id, "copying golden rootfs");
        let status = Command::new("cp")
            .args([
                "--reflink=auto",
                "--sparse=always",
                &self.fc_config.rootfs_golden,
                path_str(&self.rootfs_path)?,
            ])
            .status()
            .await
            .context("spawning cp")?;
        ensure!(status.success(), "cp --reflink=auto --sparse=always failed");
        Ok(())
    }

    /// Create a per-VM TAP device with a unique subnet.
    async fn create_tap(&self) -> anyhow::Result<()> {
        tracing::info!(
            vm_id = %self.vm_id,
            tap = %self.tap_name,
            host_ip = %self.host_ip,
            guest_ip = %self.guest_ip,
            "creating per-VM TAP device"
        );

        // Delete if exists from a previous crashed VM
        let _ = netlink::delete_link(&self.tap_name).await;

        netlink::create_tap(&self.tap_name)
            .await
            .context("creating TAP device")?;

        let ip: std::net::Ipv4Addr = self.host_ip.parse().context("parsing host IP")?;
        netlink::add_address_v4(&self.tap_name, ip, 24)
            .await
            .context("assigning IP to TAP")?;

        netlink::set_link_up(&self.tap_name)
            .await
            .context("bringing TAP up")?;

        Ok(())
    }

    /// Destroy the per-VM TAP device.
    async fn destroy_tap(&self) {
        tracing::info!(vm_id = %self.vm_id, tap = %self.tap_name, "destroying TAP device");
        let _ = netlink::delete_link(&self.tap_name).await;
    }

    /// Inject environment variables into the rootfs via loop mount (legacy mode).
    /// In overlay mode, writes to the per-VM overlay ext4 instead of the rootfs copy.
    async fn inject_env_mount(&self, env_content: &str) -> anyhow::Result<()> {
        let rootfs = if self.use_overlay() {
            path_str(&self.overlay_path)?
        } else {
            path_str(&self.rootfs_path)?
        };
        let mnt = path_str(&self.mount_point)?;

        tokio::fs::create_dir_all(&self.mount_point).await?;

        mount_loop_ext4(rootfs, mnt).context("mounting rootfs")?;

        // In overlay mode, files must go under root/ subdirectory because
        // overlay-init uses upperdir=/overlay/root (not /overlay itself)
        let write_base = if self.use_overlay() {
            self.mount_point.join("root")
        } else {
            self.mount_point.clone()
        };

        // Write environment file
        let env_dir = write_base.join("etc");
        tokio::fs::create_dir_all(&env_dir).await?;

        let env_path = env_dir.join("fc-runner-env");
        tokio::fs::write(&env_path, env_content).await?;

        // Restrict permissions on the env file (contains token)
        std::fs::set_permissions(
            &env_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;

        // Write per-VM guest network config (unique IP/gateway per slot)
        self.write_network_config_to(&write_base).await?;
        self.write_cache_service_config(&write_base).await?;

        if self.use_overlay() {
            // Override fstab (root is overlayfs, not ext4 block device)
            self.write_overlay_fstab(&write_base).await?;
            // Write entrypoint with shell fallback (agent can't read MMDS in --no-api mode)
            self.write_mount_mode_entrypoint(&write_base).await?;
            // Mask services that block boot — we pre-configure network in overlay-init
            self.mask_overlay_services(&write_base).await?;
        }

        self.umount_with_retry().await?;
        let _ = tokio::fs::remove_dir(&self.mount_point).await;
        Ok(())
    }

    /// Inject network config only (no env vars) — used in MMDS mode where
    /// secrets go via MMDS but network config still needs to be on disk.
    /// In overlay mode, writes to the per-VM overlay ext4 instead of the rootfs copy.
    async fn inject_network_config(&self) -> anyhow::Result<()> {
        let image_to_mount = if self.use_overlay() {
            path_str(&self.overlay_path)?
        } else {
            path_str(&self.rootfs_path)?
        };
        let mnt = path_str(&self.mount_point)?;

        tokio::fs::create_dir_all(&self.mount_point).await?;
        mount_loop_ext4(image_to_mount, mnt).context("mounting for network config")?;

        // In overlay mode, files must go under root/ subdirectory because
        // overlay-init uses upperdir=/overlay/root
        let write_base = if self.use_overlay() {
            self.mount_point.join("root")
        } else {
            self.mount_point.clone()
        };

        self.write_network_config_to(&write_base).await?;
        self.write_cache_service_config(&write_base).await?;

        if self.use_overlay() {
            self.write_overlay_fstab(&write_base).await?;
            self.mask_overlay_services(&write_base).await?;
        }

        self.umount_with_retry().await?;
        let _ = tokio::fs::remove_dir(&self.mount_point).await;
        Ok(())
    }

    /// Write cache service config to the VM rootfs so the entrypoint can
    /// set up `ACTIONS_CACHE_URL` and `ACTIONS_RUNTIME_TOKEN` via a runner hook.
    async fn write_cache_service_config(&self, write_base: &std::path::Path) -> anyhow::Result<()> {
        if let (Some(token), Some(port)) = (&self.cache_service_token, self.cache_service_port) {
            let etc_dir = write_base.join("etc");
            tokio::fs::create_dir_all(&etc_dir).await?;
            let config = format!(
                "FC_CACHE_URL=http://{}:{}/\nFC_CACHE_TOKEN={}\n",
                self.host_ip, port, token
            );
            tokio::fs::write(etc_dir.join("fc-runner-cache"), config).await?;
        }
        Ok(())
    }

    /// Write systemd-networkd config for guest networking.
    async fn write_network_config_to(&self, mount_point: &std::path::Path) -> anyhow::Result<()> {
        let network_dir = mount_point.join("etc/systemd/network");
        tokio::fs::create_dir_all(&network_dir).await?;
        let dns_entries: String = self
            .network_dns
            .iter()
            .map(|d| format!("DNS={}", d))
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(
            network_dir.join("20-eth.network"),
            format!(
                "[Match]\nName=eth0\n\n[Network]\nAddress={}/24\nGateway={}\n{}\n",
                self.guest_ip, self.host_ip, dns_entries
            ),
        )
        .await?;

        // Write static resolv.conf — the squashfs may have a dangling symlink to
        // /run/systemd/resolve/stub-resolv.conf which only works if systemd-resolved
        // is running. This static file ensures DNS works immediately at boot.
        let resolv_entries: String = self
            .network_dns
            .iter()
            .map(|d| format!("nameserver {}", d))
            .collect::<Vec<_>>()
            .join("\n");
        let etc_dir = mount_point.join("etc");
        // Remove any existing resolv.conf (might be a symlink in the overlay)
        let resolv_path = etc_dir.join("resolv.conf");
        let _ = tokio::fs::remove_file(&resolv_path).await;
        tokio::fs::write(&resolv_path, format!("{}\n", resolv_entries)).await?;

        // Enable systemd-resolved via symlink (if not already enabled in squashfs)
        let wants_dir = mount_point.join("etc/systemd/system/multi-user.target.wants");
        tokio::fs::create_dir_all(&wants_dir).await?;
        let resolved_link = wants_dir.join("systemd-resolved.service");
        if !resolved_link.exists() {
            let _ = tokio::fs::symlink(
                "/usr/lib/systemd/system/systemd-resolved.service",
                &resolved_link,
            )
            .await;
        }

        Ok(())
    }

    /// Write a corrected fstab for overlay mode.
    /// The squashfs has fstab pointing to /dev/vda as ext4, but in overlay mode
    /// /dev/vda is squashfs and / is overlayfs. systemd-remount-fs reads fstab
    /// and would fail trying to remount root as ext4, potentially cascading to
    /// other services including networking.
    async fn write_overlay_fstab(&self, write_base: &std::path::Path) -> anyhow::Result<()> {
        let etc_dir = write_base.join("etc");
        tokio::fs::create_dir_all(&etc_dir).await?;
        tokio::fs::write(
            etc_dir.join("fstab"),
            "# OverlayFS mode — root is overlay, not a block device\n\
             # /dev/vda is the read-only squashfs base layer\n\
             # /dev/vdb is the writable ext4 overlay (mounted by overlay-init)\n",
        )
        .await?;
        Ok(())
    }

    /// Mask systemd services that block boot in overlay mode.
    /// Network is pre-configured by overlay-init, so systemd-networkd-wait-online
    /// would block for 120s waiting for systemd-networkd to report ready (it can't
    /// read config on overlayfs). Masking prevents the boot delay.
    async fn mask_overlay_services(&self, write_base: &std::path::Path) -> anyhow::Result<()> {
        let system_dir = write_base.join("etc/systemd/system");
        tokio::fs::create_dir_all(&system_dir).await?;

        // Mask services/sockets that block boot or conflict with overlay-init networking
        for service in [
            "systemd-networkd-wait-online.service",
            "systemd-networkd.service",
            "systemd-networkd.socket",
        ] {
            let link = system_dir.join(service);
            let _ = tokio::fs::remove_file(&link).await;
            tokio::fs::symlink("/dev/null", &link).await?;
        }
        Ok(())
    }

    /// Write entrypoint.sh for mount mode (no MMDS available).
    /// The fc-runner agent only reads MMDS, which doesn't exist in --no-api mode.
    /// This entrypoint falls back to reading /etc/fc-runner-env directly.
    async fn write_mount_mode_entrypoint(
        &self,
        write_base: &std::path::Path,
    ) -> anyhow::Result<()> {
        let entrypoint = write_base.join("entrypoint.sh");
        tokio::fs::write(
            &entrypoint,
            r#"#!/bin/bash
set -euo pipefail
exec > /var/log/runner.log 2>&1

echo "=== fc-runner entrypoint $(date) ==="
echo "Network state:"
ip addr show eth0 2>&1 || true
ip route show 2>&1 || true
echo "DNS:"
cat /etc/resolv.conf 2>&1 || true

# In mount mode (--no-api), MMDS is not available.
# Read config from /etc/fc-runner-env instead.
if [ -f /etc/fc-runner-env ]; then
    echo "Loading /etc/fc-runner-env"
    source /etc/fc-runner-env
    echo "VM_ID=${VM_ID:-unset} MODE=${RUNNER_MODE:-jit}"

    # Wait for network connectivity
    for i in $(seq 1 30); do
        if curl -sf --connect-timeout 3 --max-time 5 https://github.com > /dev/null 2>&1; then
            echo "Network ready (attempt $i)"
            break
        fi
        echo "Waiting for network ($i/30)..."
        sleep 1
    done

    cd /home/runner

    if [ "${RUNNER_MODE:-jit}" = "jit" ]; then
        echo "Starting runner (JIT mode)..."
        sudo -E -u runner ./run.sh --jitconfig "${RUNNER_TOKEN}"
    else
        EPHEMERAL_FLAG=""
        if [ "${EPHEMERAL:-true}" = "true" ]; then
            EPHEMERAL_FLAG="--ephemeral"
        fi
        echo "Registering runner (ephemeral=${EPHEMERAL:-true})..."
        sudo -E -u runner ./config.sh \
            --url "${REPO_URL}" \
            --token "${RUNNER_TOKEN}" \
            --name "${RUNNER_NAME:-fc-$(hostname)}" \
            --labels "firecracker,linux,x64" \
            $EPHEMERAL_FLAG \
            --unattended \
            --disableupdate \
            --work /home/runner/_work
        echo "Starting runner (registered mode)..."
        sudo -E -u runner ./run.sh
    fi

    echo "Runner finished, shutting down"
    reboot -f
else
    echo "ERROR: /etc/fc-runner-env not found and MMDS not available"
    sleep 5
    reboot -f
fi
"#,
        )
        .await?;
        std::fs::set_permissions(
            &entrypoint,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
        Ok(())
    }

    /// Attempt umount with retries, falling back to lazy unmount.
    async fn umount_with_retry(&self) -> anyhow::Result<()> {
        let mnt = path_str(&self.mount_point)?;

        for attempt in 0..3 {
            if try_umount(mnt) {
                return Ok(());
            }
            if attempt < 2 {
                tracing::warn!(vm_id = %self.vm_id, attempt, "umount failed, retrying...");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }

        // Fallback: lazy unmount to prevent leaked mount
        tracing::warn!(vm_id = %self.vm_id, "falling back to lazy umount");
        lazy_umount_sync(mnt).context("lazy umount failed")?;
        Ok(())
    }

    /// Mount the VM rootfs after exit and dump /var/log/runner.log for debugging.
    /// In overlay mode, the log is in the per-VM overlay ext4 (upper dir).
    async fn dump_guest_log(&self) {
        // Determine which image to mount for log retrieval
        let rootfs_location = if self.use_overlay() {
            // In overlay mode, guest writes go to the overlay ext4
            self.overlay_path.clone()
        } else if self.fc_config.jailer_path.is_some() {
            let name = match filename_str(&self.rootfs_path) {
                Ok(n) => n,
                Err(_) => return,
            };
            self.jailer_root_dir().join(name)
        } else {
            self.rootfs_path.clone()
        };
        let rootfs = match path_str(&rootfs_location) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mnt = match path_str(&self.mount_point) {
            Ok(s) => s.to_string(),
            Err(_) => return,
        };

        if tokio::fs::create_dir_all(&self.mount_point).await.is_err() {
            return;
        }

        // noload skips journal replay so we can mount a dirty ext4 after VM kill
        if let Err(e) = mount_loop_ext4_ro(rootfs, &mnt) {
            tracing::warn!(vm_id = %self.vm_id, error = %e, "mount failed for guest log dump");
            let _ = tokio::fs::remove_dir(&self.mount_point).await;
            return;
        }

        // In overlay mode, guest files are under root/ on the overlay ext4
        let log_path = if self.use_overlay() {
            self.mount_point.join("root/var/log/runner.log")
        } else {
            self.mount_point.join("var/log/runner.log")
        };
        match tokio::fs::read_to_string(&log_path).await {
            Ok(contents) => {
                // Strip ANSI escape sequences and log each line
                let lines: Vec<&str> = contents.lines().collect();
                for line in lines.iter().take(50) {
                    let clean: String = line
                        .chars()
                        .scan(false, |in_escape, c| {
                            if *in_escape {
                                if c.is_ascii_alphabetic() {
                                    *in_escape = false;
                                }
                                Some(None)
                            } else if c == '\x1b' {
                                *in_escape = true;
                                Some(None)
                            } else {
                                Some(Some(c))
                            }
                        })
                        .flatten()
                        .collect();
                    let clean = clean.trim();
                    if !clean.is_empty() {
                        tracing::info!(vm_id = %self.vm_id, "[guest-log] {}", clean);
                    }
                }
                if lines.len() > 50 {
                    tracing::info!(vm_id = %self.vm_id, "[guest-log] ... (truncated)");
                }
            }
            Err(e) => {
                tracing::warn!(vm_id = %self.vm_id, error = %e, "could not read guest runner.log");
            }
        }

        let _ = lazy_umount_sync(&mnt);
        let _ = tokio::fs::remove_dir(&self.mount_point).await;
    }

    /// Compute the VSOCK CID for this VM (cid_base + slot).
    fn vsock_cid(&self) -> u32 {
        self.fc_config.vsock_cid_base + self.slot as u32
    }

    /// Returns the jailer chroot root directory: <chroot_base>/firecracker/<vm_id>/root
    fn jailer_root_dir(&self) -> PathBuf {
        PathBuf::from(&self.fc_config.jailer_chroot_base)
            .join("firecracker")
            .join(&self.vm_id)
            .join("root")
    }

    /// Resolve the Firecracker API socket path (differs between jailer and bare mode).
    fn api_socket_path(&self) -> PathBuf {
        if self.fc_config.jailer_path.is_some() {
            self.jailer_root_dir().join("api.sock")
        } else {
            self.socket_path.clone()
        }
    }

    /// Build a Firecracker VM config as a JSON value (for --config-file / no-api mode).
    /// Uses SDK model types for type safety.
    fn build_vm_config(&self, use_jailer: bool) -> anyhow::Result<serde_json::Value> {
        let (kernel_path, rootfs_path, log_path) = if use_jailer {
            (
                filename_str(&self.rootfs_path)?.replace(".ext4", "-kernel"),
                filename_str(&self.rootfs_path)?.to_string(),
                filename_str(&self.log_path)?.to_string(),
            )
        } else {
            (
                self.fc_config.kernel_path.clone(),
                path_str(&self.rootfs_path)?.to_string(),
                path_str(&self.log_path)?.to_string(),
            )
        };

        // Build boot args — append overlay init params when in overlay mode
        let mut boot_args = if self.use_overlay() {
            format!(
                "{} init=/sbin/overlay-init overlay_root=vdb",
                self.fc_config.boot_args
            )
        } else {
            self.fc_config.boot_args.clone()
        };

        // Append cache device param when cache is enabled
        if self.cache_path.is_some() {
            boot_args = format!("{} cache_dev={}", boot_args, self.cache_device_name());
        }

        // Build drives array — overlay mode uses two drives, cache adds a third
        let mut drives_vec: Vec<serde_json::Value> = if self.use_overlay() {
            let squashfs = if use_jailer {
                filename_str(&self.rootfs_path)?.replace(".ext4", "-rootfs.squashfs")
            } else {
                self.squashfs_path()
            };
            let overlay = if use_jailer {
                filename_str(&self.overlay_path)?.to_string()
            } else {
                path_str(&self.overlay_path)?.to_string()
            };
            vec![
                serde_json::json!({
                    "drive_id": "rootfs",
                    "path_on_host": squashfs,
                    "is_root_device": true,
                    "is_read_only": true
                }),
                serde_json::json!({
                    "drive_id": "overlay",
                    "path_on_host": overlay,
                    "is_root_device": false,
                    "is_read_only": false
                }),
            ]
        } else {
            vec![serde_json::json!({
                "drive_id": "rootfs",
                "path_on_host": rootfs_path,
                "is_root_device": true,
                "is_read_only": false
            })]
        };

        // Add cache drive when enabled
        if let Some(cache_path) = &self.cache_path {
            let cache_host_path = if use_jailer {
                format!("slot-{}-cache.ext4", self.slot)
            } else {
                path_str(cache_path)?.to_string()
            };
            drives_vec.push(serde_json::json!({
                "drive_id": "cache",
                "path_on_host": cache_host_path,
                "is_root_device": false,
                "is_read_only": false
            }));
        }

        let drives = serde_json::Value::Array(drives_vec);

        let mut config = serde_json::json!({
            "boot-source": {
                "kernel_image_path": kernel_path,
                "boot_args": boot_args
            },
            "drives": drives,
            "machine-config": {
                "vcpu_count": self.fc_config.vcpu_count,
                "mem_size_mib": self.fc_config.mem_size_mib
            },
            "network-interfaces": [{
                "iface_id": "eth0",
                "guest_mac": self.guest_mac,
                "host_dev_name": self.tap_name
            }],
            "logger": {
                "log_path": log_path,
                "level": self.fc_config.log_level
            }
        });

        // Enable MMDS when using MMDS injection mode
        if self.use_mmds() {
            config["mmds-config"] = serde_json::json!({
                "version": "V2",
                "network_interfaces": ["eth0"]
            });
        }

        // Add VSOCK device when enabled
        if self.fc_config.vsock_enabled {
            let cid = self.vsock_cid();
            let uds_path = if use_jailer {
                "vsock.sock".to_string()
            } else {
                path_str(&self.vsock_socket_path)?.to_string()
            };
            config["vsock"] = serde_json::json!({
                "guest_cid": cid,
                "uds_path": uds_path
            });
            tracing::info!(
                vm_id = %self.vm_id,
                cid,
                uds_path,
                "VSOCK device configured"
            );
        }

        Ok(config)
    }

    /// Set up the jailer chroot directory with all required files.
    async fn setup_jailer_chroot(&self) -> anyhow::Result<PathBuf> {
        let root_dir = self.jailer_root_dir();
        tracing::info!(
            vm_id = %self.vm_id,
            chroot = %root_dir.display(),
            "setting up jailer chroot"
        );
        tokio::fs::create_dir_all(&root_dir)
            .await
            .with_context(|| format!("creating jailer chroot directory: {}", root_dir.display()))?;

        // Hard-link (or copy) kernel into chroot
        let kernel_name = filename_str(&self.rootfs_path)?.replace(".ext4", "-kernel");
        let chroot_kernel = root_dir.join(&kernel_name);
        tracing::debug!(
            vm_id = %self.vm_id,
            src = %self.fc_config.kernel_path,
            dst = %chroot_kernel.display(),
            "linking kernel into chroot"
        );
        if tokio::fs::hard_link(&self.fc_config.kernel_path, &chroot_kernel)
            .await
            .is_err()
        {
            tokio::fs::copy(&self.fc_config.kernel_path, &chroot_kernel)
                .await
                .with_context(|| {
                    format!(
                        "copying kernel {} -> {}",
                        self.fc_config.kernel_path,
                        chroot_kernel.display()
                    )
                })?;
        }

        if self.use_overlay() {
            // Overlay mode: link squashfs (shared) and overlay ext4 (per-VM) into chroot
            let squashfs_src = self.squashfs_path();
            let squashfs_name =
                filename_str(&self.rootfs_path)?.replace(".ext4", "-rootfs.squashfs");
            let chroot_squashfs = root_dir.join(&squashfs_name);
            if tokio::fs::hard_link(&squashfs_src, &chroot_squashfs)
                .await
                .is_err()
            {
                tokio::fs::copy(&squashfs_src, &chroot_squashfs)
                    .await
                    .context("copying squashfs into chroot")?;
            }

            let overlay_name = filename_str(&self.overlay_path)?;
            let chroot_overlay = root_dir.join(overlay_name);
            if tokio::fs::hard_link(&self.overlay_path, &chroot_overlay)
                .await
                .is_err()
            {
                tokio::fs::copy(&self.overlay_path, &chroot_overlay)
                    .await
                    .context("copying overlay into chroot")?;
            }
        } else {
            // Legacy mode: link rootfs copy into chroot
            let rootfs_name = filename_str(&self.rootfs_path)?;
            let chroot_rootfs = root_dir.join(rootfs_name);
            tracing::debug!(
                vm_id = %self.vm_id,
                src = %self.rootfs_path.display(),
                dst = %chroot_rootfs.display(),
                "linking rootfs into chroot"
            );
            if tokio::fs::hard_link(&self.rootfs_path, &chroot_rootfs)
                .await
                .is_err()
            {
                tokio::fs::copy(&self.rootfs_path, &chroot_rootfs)
                    .await
                    .with_context(|| {
                        format!(
                            "copying rootfs {} -> {}",
                            self.rootfs_path.display(),
                            chroot_rootfs.display()
                        )
                    })?;
            }
        }

        // Link cache image into chroot (persistent, not per-VM)
        if let Some(cache_path) = &self.cache_path {
            let cache_name = format!("slot-{}-cache.ext4", self.slot);
            let chroot_cache = root_dir.join(&cache_name);
            if tokio::fs::hard_link(cache_path, &chroot_cache)
                .await
                .is_err()
            {
                tokio::fs::copy(cache_path, &chroot_cache)
                    .await
                    .context("copying cache image into chroot")?;
            }
        }

        // Create log file in chroot
        let log_name = filename_str(&self.log_path)?;
        tokio::fs::write(root_dir.join(log_name), "")
            .await
            .context("creating log file in jailer chroot")?;

        // Write VM config with chroot-relative paths
        let config = self.build_vm_config(true)?;
        let config_name = filename_str(&self.config_path)?;
        let chroot_config = root_dir.join(config_name);
        let rendered =
            serde_json::to_string_pretty(&config).context("serializing VM config for jailer")?;
        tokio::fs::write(&chroot_config, &rendered)
            .await
            .with_context(|| format!("writing VM config to {}", chroot_config.display()))?;

        // Chown all files to the jailer UID/GID so Firecracker can access
        // them after the jailer drops privileges.
        if let (Some(uid), Some(gid)) = (self.fc_config.jailer_uid, self.fc_config.jailer_gid) {
            use std::os::unix::fs::chown;
            for entry in std::fs::read_dir(&root_dir).context("reading jailer chroot directory")? {
                let entry = entry?;
                chown(entry.path(), Some(uid), Some(gid))
                    .with_context(|| format!("chown {}:{} {}", uid, gid, entry.path().display()))?;
            }
        }

        tracing::info!(
            vm_id = %self.vm_id,
            chroot = %root_dir.display(),
            "jailer chroot ready"
        );
        Ok(root_dir)
    }

    async fn write_vm_config(&self) -> anyhow::Result<()> {
        let use_jailer = self.fc_config.jailer_path.is_some();
        let config = self.build_vm_config(use_jailer)?;
        let rendered = serde_json::to_string_pretty(&config).context("serializing VM config")?;
        tokio::fs::write(&self.config_path, rendered).await?;
        Ok(())
    }

    /// Configure a running Firecracker instance via the SDK API.
    ///
    /// Calls the typed SDK methods to set machine config, boot source, drives,
    /// network interfaces, logger, MMDS, and VSOCK via the Firecracker API socket.
    async fn configure_instance(
        &self,
        instance: &mut Instance,
        env_content: &str,
    ) -> anyhow::Result<()> {
        // Machine configuration
        instance
            .put_machine_configuration(&MachineConfiguration {
                vcpu_count: self.fc_config.vcpu_count as isize,
                mem_size_mib: self.fc_config.mem_size_mib as isize,
                cpu_template: None,
                smt: None,
                track_dirty_pages: None,
                huge_pages: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("put_machine_configuration failed: {}", e))?;

        // Boot source — append overlay init params when in overlay mode
        let mut boot_args = if self.use_overlay() {
            format!(
                "{} init=/sbin/overlay-init overlay_root=vdb",
                self.fc_config.boot_args
            )
        } else {
            self.fc_config.boot_args.clone()
        };
        if self.cache_path.is_some() {
            boot_args = format!("{} cache_dev={}", boot_args, self.cache_device_name());
        }
        instance
            .put_guest_boot_source(&BootSource {
                kernel_image_path: PathBuf::from(&self.fc_config.kernel_path),
                boot_args: Some(boot_args),
                initrd_path: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("put_guest_boot_source failed: {}", e))?;

        if self.use_overlay() {
            // Overlay mode: squashfs (read-only root) + overlay ext4 (read-write)
            instance
                .put_guest_drive_by_id(&Drive {
                    drive_id: "rootfs".to_string(),
                    path_on_host: PathBuf::from(self.squashfs_path()),
                    is_root_device: true,
                    is_read_only: true,
                    partuuid: None,
                    cache_type: None,
                    rate_limiter: None,
                    io_engine: None,
                    socket: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_guest_drive_by_id (squashfs) failed: {}", e))?;

            instance
                .put_guest_drive_by_id(&Drive {
                    drive_id: "overlay".to_string(),
                    path_on_host: self.overlay_path.clone(),
                    is_root_device: false,
                    is_read_only: false,
                    partuuid: None,
                    cache_type: None,
                    rate_limiter: None,
                    io_engine: None,
                    socket: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_guest_drive_by_id (overlay) failed: {}", e))?;

            tracing::info!(vm_id = %self.vm_id, "overlay drives configured: squashfs (ro) + overlay ext4 (rw)");
        } else {
            // Legacy mode: single read-write rootfs
            instance
                .put_guest_drive_by_id(&Drive {
                    drive_id: "rootfs".to_string(),
                    path_on_host: self.rootfs_path.clone(),
                    is_root_device: true,
                    is_read_only: false,
                    partuuid: None,
                    cache_type: None,
                    rate_limiter: None,
                    io_engine: None,
                    socket: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_guest_drive_by_id failed: {}", e))?;
        }

        // Cache drive (persistent per-slot)
        if let Some(cache_path) = &self.cache_path {
            instance
                .put_guest_drive_by_id(&Drive {
                    drive_id: "cache".to_string(),
                    path_on_host: cache_path.clone(),
                    is_root_device: false,
                    is_read_only: false,
                    partuuid: None,
                    cache_type: None,
                    rate_limiter: None,
                    io_engine: None,
                    socket: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_guest_drive_by_id (cache) failed: {}", e))?;
            tracing::info!(
                vm_id = %self.vm_id,
                cache_dev = self.cache_device_name(),
                path = %cache_path.display(),
                "cache drive configured"
            );
        }

        // Network interface
        instance
            .put_guest_network_interface_by_id(&NetworkInterface {
                iface_id: "eth0".to_string(),
                guest_mac: Some(self.guest_mac.clone()),
                host_dev_name: PathBuf::from(&self.tap_name),
                rx_rate_limiter: None,
                tx_rate_limiter: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("put_guest_network_interface_by_id failed: {}", e))?;

        // Logger
        instance
            .put_logger(&Logger {
                log_path: self.log_path.clone(),
                level: Some(log_level_from_str(&self.fc_config.log_level)),
                show_level: None,
                show_log_origin: None,
                module: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("put_logger failed: {}", e))?;

        // MMDS configuration and metadata
        if self.use_mmds() {
            instance
                .put_mmds_config(&MmdsConfig {
                    version: Some(MmdsConfigVersion::V2),
                    ipv4_address: None,
                    network_interfaces: vec!["eth0".to_string()],
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_mmds_config failed: {}", e))?;

            // Send MMDS metadata via raw HTTP on the API socket.
            // The SDK's put_mmds() double-serializes the JSON (wraps it in quotes)
            // because MmdsContentsObject is a String alias and serde_json::to_vec
            // re-encodes it as a JSON string literal.
            let mmds_json = self.build_mmds_payload(env_content)?;
            let socket_path = self.api_socket_path();
            put_mmds_raw(&socket_path, &mmds_json).await?;

            tracing::info!(vm_id = %self.vm_id, "MMDS metadata injected via SDK");
        }

        // VSOCK device — use raw HTTP API instead of SDK because the SDK
        // incorrectly deserializes Firecracker's `{}` response as unit struct.
        if self.fc_config.vsock_enabled {
            let cid = self.vsock_cid();
            let api_sock = self.api_socket_path();
            let vsock_uds = path_str(&self.vsock_socket_path)?;
            let vsock_json = serde_json::json!({
                "guest_cid": cid,
                "uds_path": vsock_uds
            })
            .to_string();
            put_vsock_raw(&api_sock, &vsock_json).await?;
            tracing::info!(vm_id = %self.vm_id, cid, vsock_uds, "VSOCK device configured");
        }

        Ok(())
    }

    /// Build the MMDS metadata JSON string from env_content (KEY=VALUE lines).
    fn build_mmds_payload(&self, env_content: &str) -> anyhow::Result<MmdsContentsObject> {
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

    /// Create an SDK Instance using FirecrackerOption or JailerOption.
    fn build_sdk_instance(&self) -> anyhow::Result<Instance> {
        let use_jailer = self.fc_config.jailer_path.is_some();
        let sock_name = "api.sock";

        if use_jailer {
            let jailer_path = self.fc_config.jailer_path.as_ref().unwrap();
            let uid = self.fc_config.jailer_uid.expect("validated in config") as usize;
            let gid = self.fc_config.jailer_gid.expect("validated in config") as usize;

            let mut fc_opt = FirecrackerOption::new(&self.fc_config.binary_path);
            fc_opt.api_sock(sock_name);

            let mut jailer_opt = JailerOption::new(
                jailer_path,
                &self.fc_config.binary_path,
                &self.vm_id,
                gid,
                uid,
            );
            jailer_opt
                .chroot_base_dir(Some(&self.fc_config.jailer_chroot_base))
                .firecracker_option(Some(&fc_opt))
                .stdout("/dev/null")
                .stderr("/dev/null")
                .remove_jailer_workspace_dir();

            let instance = jailer_opt
                .build()
                .map_err(|e| anyhow::anyhow!("building jailer instance: {}", e))?;

            tracing::info!(
                vm_id = %self.vm_id,
                jailer = %jailer_path,
                uid, gid,
                chroot_base = %self.fc_config.jailer_chroot_base,
                "SDK instance created with jailer"
            );

            Ok(instance)
        } else {
            // Each VM needs its own socket — use the VM-specific socket_path
            // (with_file_name would make all VMs share "api.sock", causing collisions)
            let socket_path = &self.socket_path;
            let mut fc_opt = FirecrackerOption::new(&self.fc_config.binary_path);
            fc_opt
                .api_sock(socket_path)
                .id(&self.vm_id)
                .stdout("/dev/null")
                .stderr("/dev/null");

            let instance = fc_opt
                .build()
                .map_err(|e| anyhow::anyhow!("building firecracker instance: {}", e))?;

            tracing::info!(
                vm_id = %self.vm_id,
                socket = %socket_path.display(),
                "SDK instance created (bare firecracker)"
            );

            Ok(instance)
        }
    }

    /// Wait for the firecracker process to exit by polling the PID.
    /// Kills the process immediately if the cancellation token fires (graceful shutdown).
    async fn wait_for_exit(&self, instance: &Instance) -> anyhow::Result<()> {
        let pid = instance
            .firecracker_pid()
            .ok_or_else(|| anyhow::anyhow!("no firecracker PID available"))?;

        tracing::info!(vm_id = %self.vm_id, pid, "waiting for firecracker process to exit");

        let wait_fut = async {
            loop {
                if !is_process_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };

        tokio::select! {
            result = timeout(Duration::from_secs(self.vm_timeout_secs), wait_fut) => {
                result.context("VM execution timed out")?;
                tracing::info!(vm_id = %self.vm_id, pid, "firecracker process exited");
            }
            _ = self.cancel.cancelled() => {
                tracing::info!(vm_id = %self.vm_id, pid, "shutdown signal, killing firecracker process");
                kill_process(pid);
                // Give it a moment to die
                tokio::time::sleep(Duration::from_millis(200)).await;
                if is_process_alive(pid) {
                    tracing::warn!(vm_id = %self.vm_id, pid, "SIGKILL after SIGTERM failed to kill process");
                }
            }
        }
        Ok(())
    }

    /// Start Firecracker via SDK with API socket (MMDS mode).
    async fn run_with_sdk(&self, env_content: &str) -> anyhow::Result<()> {
        let mut instance = self.build_sdk_instance()?;

        // Pre-create log file so Firecracker can open it
        tokio::fs::write(&self.log_path, "")
            .await
            .context("creating log file")?;

        // start_vmm spawns the process and connects to the API socket
        instance
            .start_vmm()
            .await
            .map_err(|e| anyhow::anyhow!("start_vmm failed: {}", e))?;

        // Configure the VM via typed API calls
        if let Err(e) = self.configure_instance(&mut instance, env_content).await {
            tracing::error!(vm_id = %self.vm_id, error = %e, "VM configuration failed");
            let _ = instance.stop().await;
            return Err(e);
        }

        // Boot the VM
        instance
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("instance start failed: {}", e))?;

        tracing::info!(vm_id = %self.vm_id, "VM booted via SDK");

        // Wait for the VM process to exit
        self.wait_for_exit(&instance).await?;

        // Instance Drop will handle process cleanup (SIGTERM + file removal)
        Ok(())
    }

    /// Start Firecracker in --no-api mode (legacy mount injection).
    /// Kills the child process on shutdown signal for fast restart.
    async fn run_no_api(&self) -> anyhow::Result<std::process::ExitStatus> {
        let mut child = if let Some(jailer_path) = &self.fc_config.jailer_path {
            let uid = self.fc_config.jailer_uid.expect("validated in config");
            let gid = self.fc_config.jailer_gid.expect("validated in config");
            let config_name =
                filename_str(&self.config_path).expect("config_path must have a valid filename");
            tracing::info!(
                vm_id = %self.vm_id,
                jailer = %jailer_path,
                uid, gid,
                config = %config_name,
                chroot_base = %self.fc_config.jailer_chroot_base,
                "launching via jailer (no-api mode)"
            );
            Command::new(jailer_path)
                .arg("--id")
                .arg(&self.vm_id)
                .arg("--exec-file")
                .arg(&self.fc_config.binary_path)
                .arg("--uid")
                .arg(uid.to_string())
                .arg("--gid")
                .arg(gid.to_string())
                .arg("--chroot-base-dir")
                .arg(&self.fc_config.jailer_chroot_base)
                .arg("--")
                .arg("--config-file")
                .arg(config_name)
                .arg("--no-api")
                // Suppress guest serial console output (console=ttyS0) from flooding
                // journald. Guest logs are captured via log_path and dump_guest_log().
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("spawning jailer")?
        } else {
            Command::new(&self.fc_config.binary_path)
                .arg("--config-file")
                .arg(&self.config_path)
                .arg("--no-api")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("spawning firecracker")?
        };

        let wait_fut = child.wait();

        tokio::select! {
            result = timeout(Duration::from_secs(self.vm_timeout_secs), wait_fut) => {
                let status = result
                    .context("VM execution timed out")?
                    .context("waiting for firecracker")?;
                Ok(status)
            }
            _ = self.cancel.cancelled() => {
                tracing::info!(vm_id = %self.vm_id, "shutdown signal, killing firecracker child");
                let _ = child.kill().await;
                let status = child.wait().await.context("waiting after kill")?;
                Ok(status)
            }
        }
    }

    async fn cleanup(&self) {
        tracing::info!(vm_id = %self.vm_id, "cleaning up VM artifacts");

        // Destroy per-VM TAP device
        self.destroy_tap().await;

        let console_path = self.rootfs_path.with_extension("console");
        for path in [
            &self.rootfs_path,
            &self.config_path,
            &self.socket_path,
            &self.log_path,
            &console_path,
            &self.overlay_path,
            &self.vsock_socket_path,
        ] {
            if let Err(e) = tokio::fs::remove_file(path).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(vm_id = %self.vm_id, path = %path.display(), error = %e, "CLEANUP_FAILED");
            }
        }
        let _ = tokio::fs::remove_dir(&self.mount_point).await;

        // Clean up jailer chroot if jailer was used
        // (SDK's FStack may have already cleaned some of this, but rm -rf is idempotent)
        if self.fc_config.jailer_path.is_some() {
            let chroot_dir = PathBuf::from(&self.fc_config.jailer_chroot_base)
                .join("firecracker")
                .join(&self.vm_id);
            if let Err(e) = tokio::fs::remove_dir_all(&chroot_dir).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(vm_id = %self.vm_id, path = %chroot_dir.display(), error = %e, "jailer chroot cleanup failed");
            }
        }
    }

    pub async fn execute(self, env_content: &str) -> anyhow::Result<()> {
        self.execute_with_notify(env_content, None).await
    }

    /// Execute the VM with an optional VSOCK job-completion notification channel.
    /// When provided, the channel receives a notification as soon as the guest agent
    /// reports `JobCompleted`, allowing the orchestrator to begin replacement before
    /// the VM fully shuts down.
    pub async fn execute_with_notify(
        self,
        env_content: &str,
        vsock_notify: Option<tokio::sync::mpsc::Sender<vsock::JobDoneNotification>>,
    ) -> anyhow::Result<()> {
        let result = self.prepare_and_run(env_content, vsock_notify).await;
        self.cleanup().await;
        result
    }

    async fn prepare_and_run(
        &self,
        env_content: &str,
        vsock_notify: Option<tokio::sync::mpsc::Sender<vsock::JobDoneNotification>>,
    ) -> anyhow::Result<()> {
        let mmds = self.use_mmds();
        let use_jailer = self.fc_config.jailer_path.is_some();
        tracing::info!(
            vm_id = %self.vm_id,
            job_id = self.job_id,
            slot = self.slot,
            secret_injection = if mmds { "mmds" } else { "mount" },
            jailer = use_jailer,
            "preparing VM"
        );

        if self.use_overlay() {
            // Overlay mode: create sparse overlay ext4 (no rootfs copy needed)
            self.create_overlay().await?;
        } else {
            // Legacy mode: full rootfs copy
            self.copy_rootfs().await?;
        }

        // Ensure persistent cache image exists (creates once per slot, reused across VMs)
        self.ensure_cache_image().await?;

        if mmds {
            // MMDS mode: only inject network config to disk, secrets via MMDS
            self.inject_network_config().await?;
        } else {
            // Legacy mount mode: inject everything via loop mount
            self.inject_env_mount(env_content).await?;
        }

        self.create_tap().await?;

        // Spawn VSOCK listener before VM starts (if enabled)
        let vsock_handle = if self.fc_config.vsock_enabled {
            let cid = self.vsock_cid();
            tracing::info!(vm_id = %self.vm_id, cid, "spawning VSOCK listener");
            Some(vsock::spawn_listener(self.vm_id.clone(), cid, vsock_notify))
        } else {
            None
        };

        tracing::info!(vm_id = %self.vm_id, "launching firecracker");

        let run_result = if mmds {
            // MMDS mode: use SDK for process management and API calls
            self.run_with_sdk(env_content).await
        } else {
            // No-API mode: use config file + tokio process (SDK requires API socket)
            if use_jailer {
                self.setup_jailer_chroot().await?;
            } else {
                self.write_vm_config().await?;
                tokio::fs::write(&self.log_path, "")
                    .await
                    .context("creating log file")?;
            }
            match self.run_no_api().await {
                Ok(exit_status) => {
                    use std::os::unix::process::ExitStatusExt;
                    if exit_status.success() {
                        Ok(())
                    } else if exit_status.signal().is_some() {
                        // Signal-based exit is normal when the guest does reboot -f:
                        // KVM_EXIT_SHUTDOWN causes Firecracker/jailer to exit via signal
                        tracing::info!(
                            vm_id = %self.vm_id,
                            signal = exit_status.signal(),
                            "VM exited via signal (normal for guest reboot)"
                        );
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!(
                            "firecracker exited with status {:?}",
                            exit_status.code()
                        ))
                    }
                }
                Err(e) => Err(e),
            }
        };

        // Abort VSOCK listener when VM exits
        if let Some(handle) = vsock_handle {
            handle.abort();
        }

        // Always dump guest log, regardless of how the VM exited
        self.dump_guest_log().await;

        match run_result {
            Ok(()) => {
                tracing::info!(vm_id = %self.vm_id, "VM completed successfully");
                Ok(())
            }
            Err(e) => {
                tracing::error!(vm_id = %self.vm_id, error = %e, "VM run failed");
                Err(e)
            }
        }
    }
}

/// Parse a log level string into the SDK's LogLevel enum.
fn log_level_from_str(s: &str) -> LogLevel {
    match s.to_lowercase().as_str() {
        "error" => LogLevel::Error,
        "warning" | "warn" => LogLevel::Warning,
        "info" => LogLevel::Info,
        "debug" => LogLevel::Debug,
        _ => LogLevel::Warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FirecrackerConfig, NetworkConfig};

    fn test_fc_config(overlay: bool) -> FirecrackerConfig {
        FirecrackerConfig {
            binary_path: "/usr/local/bin/firecracker".to_string(),
            kernel_path: "/opt/fc-runner/vmlinux.bin".to_string(),
            rootfs_golden: "/opt/fc-runner/runner-rootfs-golden.ext4".to_string(),
            image: None,
            vcpu_count: 2,
            mem_size_mib: 2048,
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off".to_string(),
            log_level: "Warning".to_string(),
            jailer_path: None,
            jailer_uid: None,
            jailer_gid: None,
            jailer_chroot_base: "/var/lib/fc-runner/jailer".to_string(),
            kernel_url: None,
            cloud_img_url: None,
            secret_injection: "mmds".to_string(),
            vsock_enabled: false,
            vsock_cid_base: 3,
            overlay_rootfs: overlay,
            overlay_size_mib: 512,
            cache_enabled: false,
            cache_size_mib: 2048,
            cache_dir: "/var/lib/fc-runner/cache".to_string(),
        }
    }

    fn test_network_config() -> NetworkConfig {
        NetworkConfig {
            host_ip: "172.16.0.1".to_string(),
            guest_ip: "172.16.0.2".to_string(),
            cidr: "24".to_string(),
            dns: vec!["8.8.8.8".to_string()],
            allowed_networks: vec![],
            resolved_networks: vec![],
        }
    }

    fn create_test_vm(overlay: bool) -> MicroVm {
        let fc_config = test_fc_config(overlay);
        let network = test_network_config();
        MicroVm::new(
            12345,
            &fc_config,
            &network,
            "/tmp/fc-test",
            3600,
            0,
            CancellationToken::new(),
        )
    }

    #[test]
    fn overlay_path_set_on_new() {
        let vm = create_test_vm(true);
        assert!(vm.overlay_path.to_string_lossy().contains(".overlay.ext4"));
    }

    #[test]
    fn use_overlay_returns_correct_value() {
        let vm_overlay = create_test_vm(true);
        assert!(vm_overlay.use_overlay());

        let vm_no_overlay = create_test_vm(false);
        assert!(!vm_no_overlay.use_overlay());
    }

    #[test]
    fn squashfs_path_derived_from_golden() {
        let vm = create_test_vm(true);
        assert_eq!(
            vm.squashfs_path(),
            "/opt/fc-runner/runner-rootfs-golden.squashfs"
        );
    }

    #[test]
    fn build_vm_config_legacy_single_drive() {
        let vm = create_test_vm(false);
        let config = vm.build_vm_config(false).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[0]["is_read_only"], false);
        assert!(
            !config["boot-source"]["boot_args"]
                .as_str()
                .unwrap()
                .contains("overlay-init")
        );
    }

    #[test]
    fn build_vm_config_overlay_two_drives() {
        let vm = create_test_vm(true);
        let config = vm.build_vm_config(false).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 2);

        // First drive: squashfs, read-only root
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[0]["is_root_device"], true);
        assert_eq!(drives[0]["is_read_only"], true);
        assert!(
            drives[0]["path_on_host"]
                .as_str()
                .unwrap()
                .ends_with(".squashfs")
        );

        // Second drive: overlay, read-write
        assert_eq!(drives[1]["drive_id"], "overlay");
        assert_eq!(drives[1]["is_root_device"], false);
        assert_eq!(drives[1]["is_read_only"], false);
        assert!(
            drives[1]["path_on_host"]
                .as_str()
                .unwrap()
                .contains(".overlay.ext4")
        );
    }

    #[test]
    fn build_vm_config_overlay_boot_args() {
        let vm = create_test_vm(true);
        let config = vm.build_vm_config(false).unwrap();
        let boot_args = config["boot-source"]["boot_args"].as_str().unwrap();
        assert!(boot_args.contains("init=/sbin/overlay-init"));
        assert!(boot_args.contains("overlay_root=vdb"));
        // Original args still present
        assert!(boot_args.contains("console=ttyS0"));
    }

    #[test]
    fn build_vm_config_legacy_no_overlay_boot_args() {
        let vm = create_test_vm(false);
        let config = vm.build_vm_config(false).unwrap();
        let boot_args = config["boot-source"]["boot_args"].as_str().unwrap();
        assert!(!boot_args.contains("overlay-init"));
        assert!(!boot_args.contains("overlay_root"));
    }

    fn create_test_vm_with_cache(overlay: bool) -> MicroVm {
        let mut config = test_fc_config(overlay);
        config.cache_enabled = true;
        config.cache_dir = "/var/lib/fc-runner/cache".to_string();
        MicroVm::new(
            12345,
            &config,
            &test_network_config(),
            "/tmp/fc-test",
            3600,
            0,
            CancellationToken::new(),
        )
    }

    #[test]
    fn cache_path_set_when_enabled() {
        let vm = create_test_vm_with_cache(true);
        assert!(vm.cache_path.is_some());
        assert!(
            vm.cache_path
                .unwrap()
                .to_string_lossy()
                .contains("slot-0.ext4")
        );
    }

    #[test]
    fn cache_path_none_when_disabled() {
        let vm = create_test_vm(true);
        assert!(vm.cache_path.is_none());
    }

    #[test]
    fn cache_device_name_overlay_mode() {
        let vm = create_test_vm_with_cache(true);
        assert_eq!(vm.cache_device_name(), "vdc");
    }

    #[test]
    fn cache_device_name_legacy_mode() {
        let vm = create_test_vm_with_cache(false);
        assert_eq!(vm.cache_device_name(), "vdb");
    }

    #[test]
    fn build_vm_config_overlay_with_cache_three_drives() {
        let vm = create_test_vm_with_cache(true);
        let config = vm.build_vm_config(false).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 3);
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[1]["drive_id"], "overlay");
        assert_eq!(drives[2]["drive_id"], "cache");
        assert_eq!(drives[2]["is_read_only"], false);
        assert!(
            drives[2]["path_on_host"]
                .as_str()
                .unwrap()
                .contains("slot-0.ext4")
        );
    }

    #[test]
    fn build_vm_config_legacy_with_cache_two_drives() {
        let vm = create_test_vm_with_cache(false);
        let config = vm.build_vm_config(false).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 2);
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[1]["drive_id"], "cache");
    }

    #[test]
    fn build_vm_config_cache_boot_args() {
        let vm = create_test_vm_with_cache(true);
        let config = vm.build_vm_config(false).unwrap();
        let boot_args = config["boot-source"]["boot_args"].as_str().unwrap();
        assert!(boot_args.contains("cache_dev=vdc"));
    }

    #[test]
    fn build_vm_config_no_cache_boot_args_when_disabled() {
        let vm = create_test_vm(true);
        let config = vm.build_vm_config(false).unwrap();
        let boot_args = config["boot-source"]["boot_args"].as_str().unwrap();
        assert!(!boot_args.contains("cache_dev"));
    }

    #[test]
    fn log_level_parsing() {
        assert!(matches!(log_level_from_str("error"), LogLevel::Error));
        assert!(matches!(log_level_from_str("Error"), LogLevel::Error));
        assert!(matches!(log_level_from_str("warning"), LogLevel::Warning));
        assert!(matches!(log_level_from_str("warn"), LogLevel::Warning));
        assert!(matches!(log_level_from_str("info"), LogLevel::Info));
        assert!(matches!(log_level_from_str("debug"), LogLevel::Debug));
        assert!(matches!(log_level_from_str("unknown"), LogLevel::Warning));
    }
}
