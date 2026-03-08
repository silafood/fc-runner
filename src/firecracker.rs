use std::path::PathBuf;

use anyhow::{ensure, Context};
use tokio::process::Command;
use tokio::time::{timeout, Duration};
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
    ensure!(status.success(), "mount -o loop {} {} failed", image, target);
    Ok(())
}

/// Mount an ext4 image via loop (read-only, noload for dirty fs).
fn mount_loop_ext4_ro(image: &str, target: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("mount")
        .args(["-o", "loop,ro,noload", image, target])
        .status()
        .context("running mount (ro)")?;
    ensure!(status.success(), "mount -o loop,ro {} {} failed", image, target);
    Ok(())
}

/// Try a normal umount, returns true on success.
fn try_umount(target: &str) -> bool {
    #[cfg(target_os = "linux")]
    { nix::mount::umount(target).is_ok() }
    #[cfg(not(target_os = "linux"))]
    { std::process::Command::new("umount").arg(target).status().map(|s| s.success()).unwrap_or(false) }
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
        let status = std::process::Command::new("umount").args(["-l", target]).status()?;
        ensure!(status.success(), "lazy umount failed");
        Ok(())
    }
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
    fc_config: FirecrackerConfig,
    vm_timeout_secs: u64,
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

impl MicroVm {
    pub fn new(
        job_id: u64,
        fc_config: &FirecrackerConfig,
        network: &NetworkConfig,
        work_dir: &str,
        vm_timeout_secs: u64,
        slot: usize,
    ) -> Self {
        let vm_id = format!("fc-{}-{}", job_id, Uuid::new_v4().simple());
        let base = PathBuf::from(work_dir);
        Self {
            rootfs_path: base.join(format!("{}.ext4", vm_id)),
            config_path: base.join(format!("{}.json", vm_id)),
            socket_path: base.join(format!("{}.sock", vm_id)),
            log_path: base.join(format!("{}.log", vm_id)),
            mount_point: base.join(format!("{}.mnt", vm_id)),
            job_id,
            vm_id,
            fc_config: fc_config.clone(),
            vm_timeout_secs,
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

    async fn copy_rootfs(&self) -> anyhow::Result<()> {
        tracing::info!(vm_id = %self.vm_id, "copying golden rootfs");
        let status = Command::new("cp")
            .args([
                "--reflink=auto",
                &self.fc_config.rootfs_golden,
                path_str(&self.rootfs_path)?,
            ])
            .status()
            .await
            .context("spawning cp")?;
        ensure!(status.success(), "cp --reflink=auto failed");
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

        netlink::create_tap(&self.tap_name).await
            .context("creating TAP device")?;

        let ip: std::net::Ipv4Addr = self.host_ip.parse()
            .context("parsing host IP")?;
        netlink::add_address_v4(&self.tap_name, ip, 24).await
            .context("assigning IP to TAP")?;

        netlink::set_link_up(&self.tap_name).await
            .context("bringing TAP up")?;

        Ok(())
    }

    /// Destroy the per-VM TAP device.
    async fn destroy_tap(&self) {
        tracing::info!(vm_id = %self.vm_id, tap = %self.tap_name, "destroying TAP device");
        let _ = netlink::delete_link(&self.tap_name).await;
    }

    /// Inject environment variables into the rootfs via loop mount (legacy mode).
    async fn inject_env_mount(&self, env_content: &str) -> anyhow::Result<()> {
        let rootfs = path_str(&self.rootfs_path)?;
        let mnt = path_str(&self.mount_point)?;

        tokio::fs::create_dir_all(&self.mount_point).await?;

        mount_loop_ext4(rootfs, mnt).context("mounting rootfs")?;

        // Write environment file
        let env_dir = self.mount_point.join("etc");
        tokio::fs::create_dir_all(&env_dir).await?;

        let env_path = env_dir.join("fc-runner-env");
        tokio::fs::write(&env_path, env_content).await?;

        // Restrict permissions on the env file (contains token)
        std::fs::set_permissions(
            &env_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;

        // Write per-VM guest network config (unique IP/gateway per slot)
        let network_dir = self.mount_point.join("etc/systemd/network");
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

        self.umount_with_retry().await?;
        let _ = tokio::fs::remove_dir(&self.mount_point).await;
        Ok(())
    }

    /// Inject network config only (no env vars) — used in MMDS mode where
    /// secrets go via MMDS but network config still needs to be on disk.
    async fn inject_network_config(&self) -> anyhow::Result<()> {
        let rootfs = path_str(&self.rootfs_path)?;
        let mnt = path_str(&self.mount_point)?;

        tokio::fs::create_dir_all(&self.mount_point).await?;
        mount_loop_ext4(rootfs, mnt).context("mounting rootfs")?;

        let network_dir = self.mount_point.join("etc/systemd/network");
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

        self.umount_with_retry().await?;
        let _ = tokio::fs::remove_dir(&self.mount_point).await;
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
    async fn dump_guest_log(&self) {
        let rootfs = match path_str(&self.rootfs_path) {
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

        let log_path = self.mount_point.join("var/log/runner.log");
        match tokio::fs::read_to_string(&log_path).await {
            Ok(contents) => {
                // Strip ANSI escape sequences and log each line
                let lines: Vec<&str> = contents.lines().collect();
                for line in lines.iter().take(50) {
                    let clean: String = line
                        .chars()
                        .scan(false, |in_escape, c| {
                            if *in_escape {
                                if c.is_ascii_alphabetic() { *in_escape = false; }
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

    fn build_vm_config(&self) -> anyhow::Result<serde_json::Value> {
        let mut config = serde_json::json!({
            "boot-source": {
                "kernel_image_path": self.fc_config.kernel_path,
                "boot_args": self.fc_config.boot_args
            },
            "drives": [{
                "drive_id": "rootfs",
                "path_on_host": path_str(&self.rootfs_path)?,
                "is_root_device": true,
                "is_read_only": false
            }],
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
                "log_path": path_str(&self.log_path)?,
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
            let uds_path = path_str(&self.socket_path)?;
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

    async fn write_vm_config(&self) -> anyhow::Result<()> {
        let config = self.build_vm_config()?;
        let rendered = serde_json::to_string_pretty(&config)
            .context("serializing VM config")?;
        tokio::fs::write(&self.config_path, rendered).await?;
        Ok(())
    }

    /// PUT MMDS metadata via the Firecracker API socket.
    ///
    /// The env_content is parsed as KEY=VALUE lines and placed under a
    /// "fc-runner" namespace for the guest agent to read via MMDS V2.
    async fn put_mmds(&self, env_content: &str) -> anyhow::Result<()> {
        use http_body_util::Full;
        use hyper::body::Bytes;
        use hyper::Request;
        use hyper_util::rt::TokioIo;

        // Parse env_content (KEY=VALUE lines) into a structured object
        let mut inner = serde_json::Map::new();
        for line in env_content.lines() {
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim().to_lowercase();
                let value = value.trim();
                // Parse booleans
                if value == "true" || value == "false" {
                    inner.insert(
                        key,
                        serde_json::Value::Bool(value == "true"),
                    );
                } else {
                    inner.insert(key, serde_json::Value::String(value.to_string()));
                }
            }
        }
        // Wrap under "fc-runner" namespace for the guest agent
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "fc-runner".to_string(),
            serde_json::Value::Object(inner),
        );
        let body_json = serde_json::to_string(&serde_json::Value::Object(metadata))
            .context("serializing MMDS metadata")?;

        tracing::info!(vm_id = %self.vm_id, "putting MMDS metadata via API socket");

        // Wait for the socket to appear (Firecracker creates it on startup)
        let socket_path = &self.socket_path;
        for attempt in 0..50 {
            if tokio::fs::metadata(socket_path).await.is_ok() {
                break;
            }
            if attempt == 49 {
                anyhow::bail!("Firecracker API socket did not appear at {:?}", socket_path);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let stream = tokio::net::UnixStream::connect(socket_path)
            .await
            .context("connecting to Firecracker API socket")?;

        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .context("HTTP handshake with Firecracker API")?;

        // Spawn the connection driver
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!(error = %e, "Firecracker API connection error");
            }
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/mmds")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(Full::new(Bytes::from(body_json)))
            .context("building MMDS request")?;

        let resp = sender.send_request(req).await.context("sending MMDS PUT")?;
        let status = resp.status();

        if !status.is_success() {
            let body = http_body_util::BodyExt::collect(resp.into_body())
                .await
                .map(|b| String::from_utf8_lossy(&b.to_bytes()).to_string())
                .unwrap_or_default();
            anyhow::bail!("MMDS PUT failed (HTTP {}): {}", status, body);
        }

        tracing::info!(vm_id = %self.vm_id, "MMDS metadata injected successfully");
        Ok(())
    }

    /// Start Firecracker in --no-api mode (legacy mount injection).
    async fn run_no_api(&self) -> anyhow::Result<std::process::ExitStatus> {
        let fut = if let Some(jailer_path) = &self.fc_config.jailer_path {
            let uid = self.fc_config.jailer_uid.expect("validated in config");
            let gid = self.fc_config.jailer_gid.expect("validated in config");
            tracing::info!(
                vm_id = %self.vm_id,
                jailer = %jailer_path,
                uid, gid,
                "launching via jailer (no-api mode)"
            );
            Command::new(jailer_path)
                .arg("--id").arg(&self.vm_id)
                .arg("--exec-file").arg(&self.fc_config.binary_path)
                .arg("--uid").arg(uid.to_string())
                .arg("--gid").arg(gid.to_string())
                .arg("--chroot-base-dir").arg(&self.fc_config.jailer_chroot_base)
                .arg("--")
                .arg("--config-file").arg(&self.config_path)
                .arg("--no-api")
                .status()
        } else {
            Command::new(&self.fc_config.binary_path)
                .arg("--config-file").arg(&self.config_path)
                .arg("--no-api")
                .status()
        };

        let status = timeout(Duration::from_secs(self.vm_timeout_secs), fut)
            .await
            .context("VM execution timed out")?
            .context("spawning firecracker")?;
        Ok(status)
    }

    /// Start Firecracker with API socket (MMDS mode).
    /// Returns a child process handle so we can inject MMDS data before it exits.
    async fn run_with_api(&self, env_content: &str) -> anyhow::Result<std::process::ExitStatus> {
        let socket = path_str(&self.socket_path)?;

        let mut child = if let Some(jailer_path) = &self.fc_config.jailer_path {
            let uid = self.fc_config.jailer_uid.expect("validated in config");
            let gid = self.fc_config.jailer_gid.expect("validated in config");
            tracing::info!(
                vm_id = %self.vm_id,
                jailer = %jailer_path,
                uid, gid,
                "launching via jailer (MMDS mode)"
            );
            Command::new(jailer_path)
                .arg("--id").arg(&self.vm_id)
                .arg("--exec-file").arg(&self.fc_config.binary_path)
                .arg("--uid").arg(uid.to_string())
                .arg("--gid").arg(gid.to_string())
                .arg("--chroot-base-dir").arg(&self.fc_config.jailer_chroot_base)
                .arg("--")
                .arg("--config-file").arg(&self.config_path)
                .arg("--api-sock").arg(socket)
                .spawn()
                .context("spawning firecracker via jailer")?
        } else {
            Command::new(&self.fc_config.binary_path)
                .arg("--config-file").arg(&self.config_path)
                .arg("--api-sock").arg(socket)
                .spawn()
                .context("spawning firecracker")?
        };

        // Inject MMDS metadata while the VM is booting
        if let Err(e) = self.put_mmds(env_content).await {
            tracing::error!(vm_id = %self.vm_id, error = %e, "MMDS injection failed, killing VM");
            let _ = child.kill().await;
            return Err(e);
        }

        // Wait for the VM to exit
        let status = timeout(Duration::from_secs(self.vm_timeout_secs), child.wait())
            .await
            .context("VM execution timed out")?
            .context("waiting for firecracker")?;
        Ok(status)
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
        ] {
            if let Err(e) = tokio::fs::remove_file(path).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(vm_id = %self.vm_id, path = %path.display(), error = %e, "CLEANUP_FAILED");
            }
        }
        let _ = tokio::fs::remove_dir(&self.mount_point).await;

        // Clean up jailer chroot if jailer was used
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
        let result = self.prepare_and_run(env_content).await;
        self.cleanup().await;
        result
    }

    async fn prepare_and_run(&self, env_content: &str) -> anyhow::Result<()> {
        let mmds = self.use_mmds();
        tracing::info!(
            vm_id = %self.vm_id,
            job_id = self.job_id,
            slot = self.slot,
            secret_injection = if mmds { "mmds" } else { "mount" },
            "preparing VM"
        );

        self.copy_rootfs().await?;

        if mmds {
            // MMDS mode: only inject network config to disk, secrets via MMDS
            self.inject_network_config().await?;
        } else {
            // Legacy mount mode: inject everything via loop mount
            self.inject_env_mount(env_content).await?;
        }

        self.create_tap().await?;
        self.write_vm_config().await?;

        // Pre-create log file so Firecracker can open it
        tokio::fs::write(&self.log_path, "").await
            .context("creating log file")?;

        // Spawn VSOCK listener before VM starts (if enabled)
        let vsock_handle = if self.fc_config.vsock_enabled {
            let cid = self.vsock_cid();
            tracing::info!(vm_id = %self.vm_id, cid, "spawning VSOCK listener");
            Some(vsock::spawn_listener(self.vm_id.clone(), cid))
        } else {
            None
        };

        tracing::info!(vm_id = %self.vm_id, "launching firecracker");

        let run_result = if mmds {
            self.run_with_api(env_content).await
        } else {
            self.run_no_api().await
        };

        // Abort VSOCK listener when VM exits
        if let Some(handle) = vsock_handle {
            handle.abort();
        }

        // Always dump guest log, regardless of how the VM exited
        self.dump_guest_log().await;

        match run_result {
            Ok(exit_status) => {
                tracing::info!(vm_id = %self.vm_id, code = ?exit_status.code(), "VM exited");
                if !exit_status.success() {
                    anyhow::bail!("firecracker exited with status {:?}", exit_status.code());
                }
                Ok(())
            }
            Err(e) => {
                tracing::error!(vm_id = %self.vm_id, error = %e, "VM run failed");
                Err(e)
            }
        }
    }
}
