use std::path::PathBuf;

use anyhow::{ensure, Context};
use firecracker_rs_sdk::firecracker::FirecrackerOption;
use firecracker_rs_sdk::instance::Instance;
use firecracker_rs_sdk::jailer::JailerOption;
use firecracker_rs_sdk::models::*;
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
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux (e.g. macOS for dev), check if /proc/{pid} exists
        // or just assume alive (Firecracker only runs on Linux anyway)
        std::path::Path::new(&format!("/proc/{}", pid)).exists()
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
    /// Per-VM overlay ext4 file (used only when overlay_rootfs = true).
    overlay_path: PathBuf,
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
    ) -> Self {
        let vm_id = format!("fc-{}-{}", job_id, Uuid::new_v4().simple());
        let base = PathBuf::from(work_dir);
        Self {
            rootfs_path: base.join(format!("{}.ext4", vm_id)),
            config_path: base.join(format!("{}.json", vm_id)),
            socket_path: base.join(format!("{}.sock", vm_id)),
            log_path: base.join(format!("{}.log", vm_id)),
            mount_point: base.join(format!("{}.mnt", vm_id)),
            overlay_path: base.join(format!("{}.overlay.ext4", vm_id)),
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

    fn use_overlay(&self) -> bool {
        self.fc_config.overlay_rootfs
    }

    /// Path to the shared squashfs rootfs (derived from golden rootfs path).
    fn squashfs_path(&self) -> String {
        self.fc_config.rootfs_golden.replace(".ext4", ".squashfs")
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
        self.write_network_config_to(&self.mount_point).await?;

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

        self.write_network_config_to(&self.mount_point).await?;

        self.umount_with_retry().await?;
        let _ = tokio::fs::remove_dir(&self.mount_point).await;
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
        let boot_args = if self.use_overlay() {
            format!(
                "{} init=/sbin/overlay-init overlay_root=vdb",
                self.fc_config.boot_args
            )
        } else {
            self.fc_config.boot_args.clone()
        };

        // Build drives array — overlay mode uses two drives
        let drives = if self.use_overlay() {
            let squashfs = if use_jailer {
                filename_str(&self.rootfs_path)?
                    .replace(".ext4", "-rootfs.squashfs")
            } else {
                self.squashfs_path()
            };
            let overlay = if use_jailer {
                filename_str(&self.overlay_path)?.to_string()
            } else {
                path_str(&self.overlay_path)?.to_string()
            };
            serde_json::json!([
                {
                    "drive_id": "rootfs",
                    "path_on_host": squashfs,
                    "is_root_device": true,
                    "is_read_only": true
                },
                {
                    "drive_id": "overlay",
                    "path_on_host": overlay,
                    "is_root_device": false,
                    "is_read_only": false
                }
            ])
        } else {
            serde_json::json!([{
                "drive_id": "rootfs",
                "path_on_host": rootfs_path,
                "is_root_device": true,
                "is_read_only": false
            }])
        };

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
                path_str(&self.socket_path)?.to_string()
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
            .with_context(|| {
                format!(
                    "creating jailer chroot directory: {}",
                    root_dir.display()
                )
            })?;

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
            let squashfs_name = filename_str(&self.rootfs_path)?
                .replace(".ext4", "-rootfs.squashfs");
            let chroot_squashfs = root_dir.join(&squashfs_name);
            if tokio::fs::hard_link(&squashfs_src, &chroot_squashfs)
                .await
                .is_err()
            {
                tokio::fs::copy(&squashfs_src, &chroot_squashfs).await
                    .context("copying squashfs into chroot")?;
            }

            let overlay_name = filename_str(&self.overlay_path)?;
            let chroot_overlay = root_dir.join(overlay_name);
            if tokio::fs::hard_link(&self.overlay_path, &chroot_overlay)
                .await
                .is_err()
            {
                tokio::fs::copy(&self.overlay_path, &chroot_overlay).await
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
            for entry in
                std::fs::read_dir(&root_dir).context("reading jailer chroot directory")?
            {
                let entry = entry?;
                chown(entry.path(), Some(uid), Some(gid)).with_context(|| {
                    format!("chown {}:{} {}", uid, gid, entry.path().display())
                })?;
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
        let rendered =
            serde_json::to_string_pretty(&config).context("serializing VM config")?;
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
        let boot_args = if self.use_overlay() {
            format!(
                "{} init=/sbin/overlay-init overlay_root=vdb",
                self.fc_config.boot_args
            )
        } else {
            self.fc_config.boot_args.clone()
        };
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

            let mmds_json = self.build_mmds_payload(env_content)?;
            instance
                .put_mmds(&mmds_json)
                .await
                .map_err(|e| anyhow::anyhow!("put_mmds failed: {}", e))?;

            tracing::info!(vm_id = %self.vm_id, "MMDS metadata injected via SDK");
        }

        // VSOCK device
        if self.fc_config.vsock_enabled {
            let cid = self.vsock_cid();
            instance
                .put_guest_vsock(&Vsock {
                    guest_cid: cid,
                    uds_path: PathBuf::from("vsock.sock"),
                    vsock_id: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_guest_vsock failed: {}", e))?;

            tracing::info!(vm_id = %self.vm_id, cid, "VSOCK device configured via SDK");
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
        metadata.insert(
            "fc-runner".to_string(),
            serde_json::Value::Object(inner),
        );
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
            let socket_path = self.socket_path.with_file_name(sock_name);
            let mut fc_opt = FirecrackerOption::new(&self.fc_config.binary_path);
            fc_opt.api_sock(&socket_path).id(&self.vm_id);

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
    async fn wait_for_exit(&self, instance: &Instance) -> anyhow::Result<()> {
        let pid = instance.firecracker_pid().ok_or_else(|| {
            anyhow::anyhow!("no firecracker PID available")
        })?;

        tracing::info!(vm_id = %self.vm_id, pid, "waiting for firecracker process to exit");

        let wait_fut = async {
            loop {
                if !is_process_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };

        timeout(Duration::from_secs(self.vm_timeout_secs), wait_fut)
            .await
            .context("VM execution timed out")?;

        tracing::info!(vm_id = %self.vm_id, pid, "firecracker process exited");
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
    async fn run_no_api(&self) -> anyhow::Result<std::process::ExitStatus> {
        let fut = if let Some(jailer_path) = &self.fc_config.jailer_path {
            let uid = self.fc_config.jailer_uid.expect("validated in config");
            let gid = self.fc_config.jailer_gid.expect("validated in config");
            let config_name = filename_str(&self.config_path)
                .expect("config_path must have a valid filename");
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
                .status()
        } else {
            Command::new(&self.fc_config.binary_path)
                .arg("--config-file")
                .arg(&self.config_path)
                .arg("--no-api")
                .status()
        };

        let status = timeout(Duration::from_secs(self.vm_timeout_secs), fut)
            .await
            .context("VM execution timed out")?
            .context("spawning firecracker")?;
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
            &self.overlay_path,
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
        let result = self.prepare_and_run(env_content).await;
        self.cleanup().await;
        result
    }

    async fn prepare_and_run(&self, env_content: &str) -> anyhow::Result<()> {
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
            Some(vsock::spawn_listener(self.vm_id.clone(), cid))
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
                    if !exit_status.success() {
                        Err(anyhow::anyhow!(
                            "firecracker exited with status {:?}",
                            exit_status.code()
                        ))
                    } else {
                        Ok(())
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
            cloud_img_url: None,
            secret_injection: "mmds".to_string(),
            vsock_enabled: false,
            vsock_cid_base: 3,
            overlay_rootfs: overlay,
            overlay_size_mib: 512,
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
        MicroVm::new(12345, &fc_config, &network, "/tmp/fc-test", 3600, 0)
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
        assert!(!config["boot-source"]["boot_args"]
            .as_str()
            .unwrap()
            .contains("overlay-init"));
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
        assert!(drives[0]["path_on_host"]
            .as_str()
            .unwrap()
            .ends_with(".squashfs"));

        // Second drive: overlay, read-write
        assert_eq!(drives[1]["drive_id"], "overlay");
        assert_eq!(drives[1]["is_root_device"], false);
        assert_eq!(drives[1]["is_read_only"], false);
        assert!(drives[1]["path_on_host"]
            .as_str()
            .unwrap()
            .contains(".overlay.ext4"));
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
