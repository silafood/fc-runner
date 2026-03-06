use std::path::Path;

use anyhow::{ensure, Context};
use tokio::process::Command;

use crate::config::AppConfig;

const KERNEL_URL: &str =
    "https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/x86_64/kernels/vmlinux.bin";
const RUNNER_VERSION: &str = "2.323.0";
const ROOTFS_SIZE_MIB: u32 = 4096;

/// Ensures the kernel and golden rootfs exist, downloading/building them if missing.
pub async fn ensure_vm_assets(config: &AppConfig) -> anyhow::Result<()> {
    ensure_kernel(&config.firecracker.kernel_path).await?;
    ensure_golden_rootfs(&config.firecracker.rootfs_golden).await?;
    Ok(())
}

async fn ensure_kernel(kernel_path: &str) -> anyhow::Result<()> {
    if Path::new(kernel_path).exists() {
        tracing::info!(path = kernel_path, "kernel already exists");
        return Ok(());
    }

    tracing::info!(path = kernel_path, "kernel not found, downloading...");

    if let Some(parent) = Path::new(kernel_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let status = Command::new("curl")
        .args(["-fsSL", "-o", kernel_path, KERNEL_URL])
        .status()
        .await
        .context("spawning curl for kernel download")?;
    ensure!(status.success(), "failed to download kernel from {}", KERNEL_URL);

    tracing::info!(path = kernel_path, "kernel downloaded");
    Ok(())
}

async fn ensure_golden_rootfs(rootfs_path: &str) -> anyhow::Result<()> {
    if Path::new(rootfs_path).exists() {
        tracing::info!(path = rootfs_path, "golden rootfs already exists");
        return Ok(());
    }

    tracing::info!(path = rootfs_path, "golden rootfs not found, building...");

    if let Some(parent) = Path::new(rootfs_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Create empty ext4 image
    let status = Command::new("dd")
        .args([
            "if=/dev/zero",
            &format!("of={}", rootfs_path),
            "bs=1M",
            &format!("count={}", ROOTFS_SIZE_MIB),
            "status=progress",
        ])
        .status()
        .await
        .context("creating rootfs image")?;
    ensure!(status.success(), "dd failed");

    let status = Command::new("mkfs.ext4")
        .args(["-F", rootfs_path])
        .status()
        .await
        .context("mkfs.ext4")?;
    ensure!(status.success(), "mkfs.ext4 failed");

    // Mount, debootstrap, configure
    let mount_dir = format!("{}.mnt", rootfs_path);
    tokio::fs::create_dir_all(&mount_dir).await?;

    let status = Command::new("mount")
        .args(["-o", "loop", rootfs_path, &mount_dir])
        .status()
        .await
        .context("mounting rootfs image")?;
    ensure!(status.success(), "mount failed");

    // Run the build inside a helper script so we can clean up on failure
    let result = build_rootfs_contents(&mount_dir).await;

    // Always unmount
    let umount_status = Command::new("umount")
        .arg(&mount_dir)
        .status()
        .await
        .context("unmounting rootfs")?;

    let _ = tokio::fs::remove_dir(&mount_dir).await;

    if let Err(e) = result {
        // Clean up the partial image
        let _ = tokio::fs::remove_file(rootfs_path).await;
        return Err(e).context("building rootfs contents");
    }
    ensure!(umount_status.success(), "umount failed");

    tracing::info!(path = rootfs_path, "golden rootfs built");
    Ok(())
}

async fn build_rootfs_contents(mount_dir: &str) -> anyhow::Result<()> {
    // Debootstrap Ubuntu 24.04 Noble
    tracing::info!("running debootstrap (this takes a few minutes)...");
    let status = Command::new("debootstrap")
        .args([
            "--arch=amd64",
            "--include=systemd,systemd-sysv,curl,git,jq,ca-certificates,sudo,openssh-client,unzip,libicu74,liblttng-ust1",
            "noble",
            mount_dir,
            "http://archive.ubuntu.com/ubuntu",
        ])
        .status()
        .await
        .context("debootstrap")?;
    ensure!(status.success(), "debootstrap failed");

    // Network configuration
    let network_dir = format!("{}/etc/systemd/network", mount_dir);
    tokio::fs::create_dir_all(&network_dir).await?;
    tokio::fs::write(
        format!("{}/20-eth.network", network_dir),
        "[Match]\nName=eth0\n\n[Network]\nAddress=172.16.0.2/24\nGateway=172.16.0.1\nDNS=8.8.8.8\n",
    )
    .await?;

    // Enable systemd-networkd
    let status = Command::new("chroot")
        .args([mount_dir, "systemctl", "enable", "systemd-networkd", "systemd-resolved"])
        .status()
        .await?;
    ensure!(status.success(), "enabling systemd-networkd failed");

    // Create runner user
    let _ = Command::new("chroot")
        .args([mount_dir, "useradd", "-m", "-s", "/bin/bash", "runner"])
        .status()
        .await;

    tokio::fs::write(
        format!("{}/etc/sudoers.d/runner", mount_dir),
        "runner ALL=(ALL) NOPASSWD:ALL\n",
    )
    .await?;

    // Download and install GitHub Actions runner
    tracing::info!("installing GitHub Actions runner v{}...", RUNNER_VERSION);
    let runner_dir = format!("{}/home/runner", mount_dir);
    tokio::fs::create_dir_all(&runner_dir).await?;

    let runner_url = format!(
        "https://github.com/actions/runner/releases/download/v{}/actions-runner-linux-x64-{}.tar.gz",
        RUNNER_VERSION, RUNNER_VERSION
    );
    let status = Command::new("bash")
        .args([
            "-c",
            &format!("curl -fsSL '{}' | tar xz -C '{}'", runner_url, runner_dir),
        ])
        .status()
        .await
        .context("downloading actions-runner")?;
    ensure!(status.success(), "failed to download actions-runner");

    // Install runner dependencies
    let status = Command::new("chroot")
        .args([mount_dir, "/home/runner/bin/installdependencies.sh"])
        .status()
        .await
        .context("installing runner dependencies")?;
    ensure!(status.success(), "installdependencies.sh failed");

    // Fix ownership
    let status = Command::new("chroot")
        .args([mount_dir, "chown", "-R", "runner:runner", "/home/runner"])
        .status()
        .await?;
    ensure!(status.success(), "chown failed");

    // Write entrypoint
    tokio::fs::write(
        format!("{}/entrypoint.sh", mount_dir),
        r#"#!/bin/bash
set -euo pipefail
source /run/fc-runner-env

cd /home/runner
sudo -u runner ./config.sh \
  --url "${REPO_URL}" \
  --token "${RUNNER_TOKEN}" \
  --name "fc-$(cat /proc/sys/kernel/hostname)" \
  --labels "firecracker,linux,x64" \
  --ephemeral \
  --unattended \
  --work /home/runner/_work

sudo -u runner ./run.sh
"#,
    )
    .await?;

    let status = Command::new("chmod")
        .args(["+x", &format!("{}/entrypoint.sh", mount_dir)])
        .status()
        .await?;
    ensure!(status.success(), "chmod entrypoint failed");

    // rc.local to run entrypoint on boot
    tokio::fs::write(
        format!("{}/etc/rc.local", mount_dir),
        "#!/bin/bash\n/entrypoint.sh >> /var/log/runner.log 2>&1 &\nexit 0\n",
    )
    .await?;

    let status = Command::new("chmod")
        .args(["+x", &format!("{}/etc/rc.local", mount_dir)])
        .status()
        .await?;
    ensure!(status.success(), "chmod rc.local failed");

    Ok(())
}
