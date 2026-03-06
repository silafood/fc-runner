use std::path::PathBuf;

use anyhow::{ensure, Context};
use tokio::process::Command;
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
}

impl MicroVm {
    pub fn new(job_id: u64, fc_config: &FirecrackerConfig, work_dir: &str) -> Self {
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
        }
    }

    async fn copy_rootfs(&self) -> anyhow::Result<()> {
        let status = Command::new("cp")
            .args([
                "--reflink=auto",
                &self.fc_config.rootfs_golden,
                self.rootfs_path.to_str().unwrap(),
            ])
            .status()
            .await
            .context("spawning cp")?;
        ensure!(status.success(), "cp --reflink=auto failed");
        Ok(())
    }

    async fn inject_env(&self, jit_token: &str, repo_url: &str) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.mount_point).await?;

        let status = Command::new("mount")
            .args([
                "-o",
                "loop",
                self.rootfs_path.to_str().unwrap(),
                self.mount_point.to_str().unwrap(),
            ])
            .status()
            .await
            .context("mounting rootfs")?;
        ensure!(status.success(), "mount failed");

        let env_dir = self.mount_point.join("run");
        tokio::fs::create_dir_all(&env_dir).await?;

        let env_content = format!(
            "RUNNER_TOKEN={}\nREPO_URL={}\nVM_ID={}\n",
            jit_token, repo_url, self.vm_id
        );
        tokio::fs::write(env_dir.join("fc-runner-env"), env_content).await?;

        let status = Command::new("umount")
            .arg(self.mount_point.to_str().unwrap())
            .status()
            .await
            .context("unmounting rootfs")?;
        ensure!(status.success(), "umount failed");

        let _ = tokio::fs::remove_dir(&self.mount_point).await;
        Ok(())
    }

    async fn write_vm_config(&self) -> anyhow::Result<()> {
        let template = tokio::fs::read_to_string(&self.fc_config.vm_config_template)
            .await
            .context("reading vm-config template")?;
        let rendered = template
            .replace("__KERNEL_PATH__", &self.fc_config.kernel_path)
            .replace("__ROOTFS_PATH__", self.rootfs_path.to_str().unwrap())
            .replace("__VCPU_COUNT__", &self.fc_config.vcpu_count.to_string())
            .replace("__MEM_MIB__", &self.fc_config.mem_size_mib.to_string())
            .replace("__TAP_IFACE__", &self.fc_config.tap_interface)
            .replace("__LOG_PATH__", self.log_path.to_str().unwrap())
            .replace("__VM_ID__", &self.vm_id);
        tokio::fs::write(&self.config_path, rendered).await?;
        Ok(())
    }

    async fn run(&self) -> anyhow::Result<std::process::ExitStatus> {
        let status = Command::new(&self.fc_config.binary_path)
            .arg("--config-file")
            .arg(&self.config_path)
            .arg("--no-api")
            .status()
            .await
            .context("spawning firecracker")?;
        Ok(status)
    }

    async fn cleanup(&self) {
        for path in [
            &self.rootfs_path,
            &self.config_path,
            &self.socket_path,
            &self.log_path,
        ] {
            if let Err(e) = tokio::fs::remove_file(path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), error = %e, "cleanup failed");
                }
            }
        }
        let _ = tokio::fs::remove_dir(&self.mount_point).await;
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

        tracing::info!(vm_id = %self.vm_id, "launching firecracker");
        let exit_status = self.run().await?;
        tracing::info!(vm_id = %self.vm_id, code = ?exit_status.code(), "VM exited");

        if !exit_status.success() {
            anyhow::bail!("firecracker exited with status {:?}", exit_status.code());
        }
        Ok(())
    }
}
