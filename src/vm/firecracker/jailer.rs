use std::path::PathBuf;

use anyhow::Context;

use super::MicroVm;
use super::process::filename_str;

impl MicroVm {
    /// Set up the jailer chroot directory with all required files.
    pub(crate) async fn setup_jailer_chroot(&self) -> anyhow::Result<PathBuf> {
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
}
