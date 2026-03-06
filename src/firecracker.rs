use std::path::PathBuf;

use anyhow::{ensure, Context};
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use crate::config::FirecrackerConfig;

pub struct MicroVm {
    pub vm_id: String,
    pub job_id: u64,
    rootfs_path: PathBuf,
    config_path: PathBuf,
    socket_path: PathBuf,
    log_path: PathBuf,
    mount_point: PathBuf,
    fc_config: FirecrackerConfig,
    vm_timeout_secs: u64,
}

/// Convert a PathBuf to &str with a descriptive error instead of panicking.
fn path_str(path: &PathBuf) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path contains invalid UTF-8: {}", path.display()))
}

impl MicroVm {
    pub fn new(job_id: u64, fc_config: &FirecrackerConfig, work_dir: &str, vm_timeout_secs: u64) -> Self {
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
        }
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

    async fn inject_env(&self, jit_token: &str, repo_url: &str) -> anyhow::Result<()> {
        let rootfs = path_str(&self.rootfs_path)?;
        let mnt = path_str(&self.mount_point)?;

        tokio::fs::create_dir_all(&self.mount_point).await?;

        let status = Command::new("mount")
            .args(["-o", "loop", rootfs, mnt])
            .status()
            .await
            .context("mounting rootfs")?;
        ensure!(status.success(), "mount failed");

        // Verify mount actually succeeded (TOCTOU protection)
        let mountpoint_ok = Command::new("mountpoint")
            .args(["-q", mnt])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !mountpoint_ok {
            anyhow::bail!("mount point verification failed for {}", mnt);
        }

        let env_dir = self.mount_point.join("etc");
        tokio::fs::create_dir_all(&env_dir).await?;

        let env_content = format!(
            "RUNNER_TOKEN={}\nREPO_URL={}\nVM_ID={}\n",
            jit_token, repo_url, self.vm_id
        );
        let env_path = env_dir.join("fc-runner-env");
        tokio::fs::write(&env_path, env_content).await?;

        // Restrict permissions on the env file (contains token)
        Command::new("chmod")
            .args(["0600", path_str(&env_path)?])
            .status()
            .await?;

        self.umount_with_retry().await?;
        let _ = tokio::fs::remove_dir(&self.mount_point).await;
        Ok(())
    }

    /// Attempt umount with retries, falling back to lazy unmount.
    async fn umount_with_retry(&self) -> anyhow::Result<()> {
        let mnt = path_str(&self.mount_point)?;

        for attempt in 0..3 {
            let status = Command::new("umount")
                .arg(mnt)
                .status()
                .await
                .context("unmounting rootfs")?;
            if status.success() {
                return Ok(());
            }
            if attempt < 2 {
                tracing::warn!(vm_id = %self.vm_id, attempt, "umount failed, retrying...");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }

        // Fallback: lazy unmount to prevent leaked mount
        tracing::warn!(vm_id = %self.vm_id, "falling back to lazy umount");
        let status = Command::new("umount")
            .args(["-l", mnt])
            .status()
            .await
            .context("lazy umount")?;
        ensure!(status.success(), "lazy umount failed");
        Ok(())
    }

    async fn write_vm_config(&self) -> anyhow::Result<()> {
        let template = tokio::fs::read_to_string(&self.fc_config.vm_config_template)
            .await
            .context("reading vm-config template")?;
        let rendered = template
            .replace("__KERNEL_PATH__", &self.fc_config.kernel_path)
            .replace("__ROOTFS_PATH__", path_str(&self.rootfs_path)?)
            .replace("__VCPU_COUNT__", &self.fc_config.vcpu_count.to_string())
            .replace("__MEM_MIB__", &self.fc_config.mem_size_mib.to_string())
            .replace("__TAP_IFACE__", &self.fc_config.tap_interface)
            .replace("__LOG_PATH__", path_str(&self.log_path)?)
            .replace("__VM_ID__", &self.vm_id);
        tokio::fs::write(&self.config_path, rendered).await?;
        Ok(())
    }

    async fn run(&self) -> anyhow::Result<std::process::ExitStatus> {
        let fut = if let Some(jailer_path) = &self.fc_config.jailer_path {
            let uid = self.fc_config.jailer_uid.expect("validated in config");
            let gid = self.fc_config.jailer_gid.expect("validated in config");
            tracing::info!(
                vm_id = %self.vm_id,
                jailer = %jailer_path,
                uid, gid,
                "launching via jailer"
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
                .arg(&self.config_path)
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
        for path in [
            &self.rootfs_path,
            &self.config_path,
            &self.socket_path,
            &self.log_path,
        ] {
            if let Err(e) = tokio::fs::remove_file(path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(vm_id = %self.vm_id, path = %path.display(), error = %e, "CLEANUP_FAILED");
                }
            }
        }
        let _ = tokio::fs::remove_dir(&self.mount_point).await;

        // Clean up jailer chroot if jailer was used
        if self.fc_config.jailer_path.is_some() {
            let chroot_dir = PathBuf::from(&self.fc_config.jailer_chroot_base)
                .join("firecracker")
                .join(&self.vm_id);
            if let Err(e) = tokio::fs::remove_dir_all(&chroot_dir).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(vm_id = %self.vm_id, path = %chroot_dir.display(), error = %e, "jailer chroot cleanup failed");
                }
            }
        }
    }

    pub async fn execute(self, jit_token: &str, repo_url: &str) -> anyhow::Result<()> {
        let result = self.prepare_and_run(jit_token, repo_url).await;
        self.cleanup().await;
        result
    }

    async fn prepare_and_run(&self, jit_token: &str, repo_url: &str) -> anyhow::Result<()> {
        tracing::info!(vm_id = %self.vm_id, job_id = self.job_id, "preparing VM");
        self.copy_rootfs().await?;
        self.inject_env(jit_token, repo_url).await?;
        self.write_vm_config().await?;

        // Pre-create log file so Firecracker can open it
        tokio::fs::write(&self.log_path, "").await
            .context("creating log file")?;

        tracing::info!(vm_id = %self.vm_id, "launching firecracker");
        let exit_status = self.run().await?;
        tracing::info!(vm_id = %self.vm_id, code = ?exit_status.code(), "VM exited");

        if !exit_status.success() {
            anyhow::bail!("firecracker exited with status {:?}", exit_status.code());
        }
        Ok(())
    }
}
