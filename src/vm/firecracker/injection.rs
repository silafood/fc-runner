use anyhow::{Context, ensure};
use tokio::process::Command;
use tokio::time::Duration;

use super::MicroVm;
use super::mount::{lazy_umount_sync, mount_loop_ext4, try_umount};
use super::process::path_str;

impl MicroVm {
    /// Create a per-VM sparse overlay ext4 file for OverlayFS COW mode.
    pub(crate) async fn create_overlay(&self) -> anyhow::Result<()> {
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
    pub(crate) async fn ensure_cache_image(&self) -> anyhow::Result<()> {
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

    pub(crate) async fn copy_rootfs(&self) -> anyhow::Result<()> {
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

    /// Inject environment variables into the rootfs via loop mount (legacy mode).
    /// In overlay mode, writes to the per-VM overlay ext4 instead of the rootfs copy.
    pub(crate) async fn inject_env_mount(&self, env_content: &str) -> anyhow::Result<()> {
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
    pub(crate) async fn inject_network_config(&self) -> anyhow::Result<()> {
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
    /// set up `ACTIONS_CACHE_URL`, `ACTIONS_RUNTIME_TOKEN`, and S3 credentials.
    async fn write_cache_service_config(&self, write_base: &std::path::Path) -> anyhow::Result<()> {
        if let (Some(token), Some(port)) = (&self.cache_service_token, self.cache_service_port) {
            let etc_dir = write_base.join("etc");
            tokio::fs::create_dir_all(&etc_dir).await?;
            let mut config = format!(
                "FC_CACHE_URL=http://{}:{}/\nFC_CACHE_TOKEN={}\n",
                self.host_ip, port, token
            );
            // Append S3 credentials for runs-on/cache direct uploads
            if let Some(s3) = &self.s3_config {
                config.push_str(&format!("FC_S3_ENDPOINT={}\n", s3.endpoint));
                config.push_str(&format!("FC_S3_BUCKET={}\n", s3.bucket));
                config.push_str(&format!("FC_S3_ACCESS_KEY={}\n", s3.access_key));
                config.push_str(&format!("FC_S3_SECRET_KEY={}\n", s3.secret_key));
                config.push_str(&format!("FC_S3_REGION={}\n", s3.region));
            }
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
    pub(crate) async fn umount_with_retry(&self) -> anyhow::Result<()> {
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
}
