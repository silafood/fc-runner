use std::path::Path;

use anyhow::{ensure, Context};
use tokio::process::Command;

use crate::config::{AppConfig, NetworkConfig};

const KERNEL_URL: &str =
    "https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/x86_64/kernels/vmlinux.bin";
const RUNNER_VERSION: &str = "2.323.0";
const ROOTFS_SIZE_MIB: u32 = 4096;

/// Ensures all VM prerequisites are in place: KVM, kernel, rootfs, network, and AppArmor.
pub async fn ensure_vm_assets(config: &AppConfig) -> anyhow::Result<()> {
    preflight_kvm()?;
    ensure_kernel(&config.firecracker.kernel_path).await?;
    ensure_golden_rootfs(&config.firecracker.rootfs_golden, &config.network).await?;
    ensure_network(&config.network).await?;
    ensure_apparmor(&config.firecracker.binary_path).await?;
    Ok(())
}

/// Verifies KVM is available and the current user has access.
fn preflight_kvm() -> anyhow::Result<()> {
    // Check CPU virtualization support
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let virt_count = cpuinfo
        .lines()
        .filter(|l| l.contains("vmx") || l.contains("svm"))
        .count();
    if virt_count == 0 {
        anyhow::bail!(
            "CPU virtualization (VT-x/AMD-V) not detected in /proc/cpuinfo.\n\
             Firecracker requires hardware virtualization support.\n\
             If running inside a VM, enable nested virtualization on the host."
        );
    }
    tracing::info!(virt_extensions = virt_count, "CPU virtualization support detected");

    // Check /dev/kvm exists
    let kvm_path = Path::new("/dev/kvm");
    if !kvm_path.exists() {
        anyhow::bail!(
            "/dev/kvm not found. Load the KVM module:\n\
             \n\
             Intel: sudo modprobe kvm_intel\n\
             AMD:   sudo modprobe kvm_amd"
        );
    }

    // Check current user can access /dev/kvm
    use std::fs::OpenOptions;
    match OpenOptions::new().read(true).write(true).open(kvm_path) {
        Ok(_) => {
            tracing::info!("/dev/kvm is accessible");
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let user = std::env::var("USER").unwrap_or_else(|_| "your-user".into());
            anyhow::bail!(
                "Permission denied on /dev/kvm. Add your user to the kvm group:\n\
                 \n\
                 sudo usermod -aG kvm {user}\n\
                 newgrp kvm\n\
                 \n\
                 Then restart fc-runner.",
                user = user,
            );
        }
        Err(e) => {
            anyhow::bail!("Cannot open /dev/kvm: {}", e);
        }
    }

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
        .args(["-fsSL", "--proto", "=https", "--tlsv1.2", "-o", kernel_path, KERNEL_URL])
        .status()
        .await
        .context("spawning curl for kernel download")?;
    ensure!(status.success(), "failed to download kernel");

    tracing::info!(path = kernel_path, "kernel downloaded");
    Ok(())
}

async fn ensure_golden_rootfs(rootfs_path: &str, network: &NetworkConfig) -> anyhow::Result<()> {
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

    // Verify mount succeeded
    let mountpoint_ok = Command::new("mountpoint")
        .args(["-q", &mount_dir])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    ensure!(mountpoint_ok, "mount point verification failed for {}", mount_dir);

    // Run the build inside a helper script so we can clean up on failure
    let result = build_rootfs_contents(&mount_dir, network).await;

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

async fn build_rootfs_contents(mount_dir: &str, network: &NetworkConfig) -> anyhow::Result<()> {
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
    let dns_entries: String = network
        .dns
        .iter()
        .map(|d| format!("DNS={}", d))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(
        format!("{}/20-eth.network", network_dir),
        format!(
            "[Match]\nName=eth0\n\n[Network]\nAddress={}/{}\nGateway={}\n{}\n",
            network.guest_ip, network.cidr, network.host_ip, dns_entries
        ),
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
    let runner_tarball = format!("{}/actions-runner.tar.gz", runner_dir);
    let status = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--tlsv1.2", "-o", &runner_tarball, &runner_url])
        .status()
        .await
        .context("downloading actions-runner")?;
    ensure!(status.success(), "failed to download actions-runner");

    let status = Command::new("tar")
        .args(["xzf", &runner_tarball, "-C", &runner_dir])
        .status()
        .await
        .context("extracting actions-runner")?;
    ensure!(status.success(), "failed to extract actions-runner");

    let _ = tokio::fs::remove_file(&runner_tarball).await;

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

/// Ensures TAP device exists, IP forwarding is enabled, and iptables NAT rules are in place.
async fn ensure_network(network: &NetworkConfig) -> anyhow::Result<()> {
    ensure_tap_device(network).await?;
    ensure_ip_forwarding().await?;
    ensure_nat_rules(network).await?;
    Ok(())
}

async fn ensure_tap_device(network: &NetworkConfig) -> anyhow::Result<()> {
    // Check if TAP device already exists
    let status = Command::new("ip")
        .args(["link", "show", &network.tap_device])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;

    if status.success() {
        tracing::info!(tap = %network.tap_device, "TAP device already exists");
        return Ok(());
    }

    tracing::info!(tap = %network.tap_device, "creating TAP device");

    let status = Command::new("ip")
        .args(["tuntap", "add", &network.tap_device, "mode", "tap"])
        .status()
        .await
        .context("creating TAP device")?;
    ensure!(status.success(), "ip tuntap add failed");

    let addr = format!("{}/{}", network.host_ip, network.cidr);
    let status = Command::new("ip")
        .args(["addr", "add", &addr, "dev", &network.tap_device])
        .status()
        .await
        .context("assigning IP to TAP")?;
    ensure!(status.success(), "ip addr add failed");

    let status = Command::new("ip")
        .args(["link", "set", &network.tap_device, "up"])
        .status()
        .await
        .context("bringing TAP up")?;
    ensure!(status.success(), "ip link set up failed");

    tracing::info!(
        tap = %network.tap_device,
        addr = %addr,
        "TAP device created and configured"
    );
    Ok(())
}

async fn ensure_ip_forwarding() -> anyhow::Result<()> {
    let current = tokio::fs::read_to_string("/proc/sys/net/ipv4/ip_forward").await?;
    if current.trim() == "1" {
        tracing::info!("IP forwarding already enabled");
        return Ok(());
    }

    tracing::info!("enabling IP forwarding");
    let status = Command::new("sysctl")
        .args(["-w", "net.ipv4.ip_forward=1"])
        .stdout(std::process::Stdio::null())
        .status()
        .await
        .context("enabling ip_forward")?;
    ensure!(status.success(), "sysctl ip_forward failed");
    Ok(())
}

async fn ensure_nat_rules(network: &NetworkConfig) -> anyhow::Result<()> {
    // Find default route interface
    let output = Command::new("ip")
        .args(["route"])
        .output()
        .await
        .context("reading default route")?;
    let routes = String::from_utf8_lossy(&output.stdout);
    let default_iface = routes
        .lines()
        .find(|l| l.starts_with("default"))
        .and_then(|l| l.split_whitespace().nth(4))
        .ok_or_else(|| anyhow::anyhow!("no default route found"))?
        .to_string();

    tracing::info!(iface = %default_iface, "configuring NAT rules");

    // MASQUERADE — outbound NAT
    add_iptables_rule_if_missing(&[
        "-t", "nat", "-A", "POSTROUTING",
        "-o", &default_iface,
        "-j", "MASQUERADE",
    ], &[
        "-t", "nat", "-C", "POSTROUTING",
        "-o", &default_iface,
        "-j", "MASQUERADE",
    ]).await?;

    // FORWARD — allow TAP → internet
    add_iptables_rule_if_missing(&[
        "-A", "FORWARD",
        "-i", &network.tap_device,
        "-o", &default_iface,
        "-j", "ACCEPT",
    ], &[
        "-C", "FORWARD",
        "-i", &network.tap_device,
        "-o", &default_iface,
        "-j", "ACCEPT",
    ]).await?;

    // FORWARD — allow established return traffic
    add_iptables_rule_if_missing(&[
        "-A", "FORWARD",
        "-i", &default_iface,
        "-o", &network.tap_device,
        "-m", "state", "--state", "RELATED,ESTABLISHED",
        "-j", "ACCEPT",
    ], &[
        "-C", "FORWARD",
        "-i", &default_iface,
        "-o", &network.tap_device,
        "-m", "state", "--state", "RELATED,ESTABLISHED",
        "-j", "ACCEPT",
    ]).await?;

    tracing::info!("NAT rules configured");
    Ok(())
}

async fn add_iptables_rule_if_missing(add_args: &[&str], check_args: &[&str]) -> anyhow::Result<()> {
    // Check if rule exists
    let status = Command::new("iptables")
        .args(check_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;

    if status.success() {
        return Ok(());
    }

    // Add the rule
    let status = Command::new("iptables")
        .args(add_args)
        .status()
        .await
        .context("adding iptables rule")?;
    ensure!(status.success(), "iptables rule failed: {:?}", add_args);
    Ok(())
}

/// Loads and enforces AppArmor profiles for fc-runner and Firecracker if AppArmor is available.
async fn ensure_apparmor(firecracker_binary: &str) -> anyhow::Result<()> {
    // Check if AppArmor is enabled on this system
    if !Path::new("/sys/module/apparmor").exists() {
        tracing::info!("AppArmor not available on this system, skipping profile enforcement");
        return Ok(());
    }

    // Check if aa-enforce is installed
    let which = Command::new("which")
        .arg("aa-enforce")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;

    if !which.map(|s| s.success()).unwrap_or(false) {
        tracing::warn!(
            "apparmor-utils not installed, skipping profile enforcement. \
             Install with: sudo apt install -y apparmor-utils"
        );
        return Ok(());
    }

    let profiles = [
        ("/etc/apparmor.d/usr.local.bin.firecracker", firecracker_binary),
        ("/etc/apparmor.d/usr.local.bin.fc-runner", "/usr/local/bin/fc-runner"),
    ];

    for (profile_path, binary) in &profiles {
        if !Path::new(profile_path).exists() {
            tracing::info!(
                profile = profile_path,
                "AppArmor profile not installed, skipping. \
                 Copy from apparmor/ directory to /etc/apparmor.d/"
            );
            continue;
        }

        // Check if already enforced via aa-status
        let output = Command::new("aa-status")
            .arg("--json")
            .output()
            .await;

        let already_enforced = output
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.contains(binary))
            .unwrap_or(false);

        if already_enforced {
            tracing::info!(profile = profile_path, "AppArmor profile already loaded");
            continue;
        }

        // Load and enforce the profile
        tracing::info!(profile = profile_path, "loading AppArmor profile");
        let status = Command::new("apparmor_parser")
            .args(["-r", "-W", profile_path])
            .status()
            .await
            .context("loading AppArmor profile")?;

        if !status.success() {
            tracing::warn!(
                profile = profile_path,
                "failed to load AppArmor profile, continuing without enforcement"
            );
            continue;
        }

        let status = Command::new("aa-enforce")
            .arg(profile_path)
            .status()
            .await
            .context("enforcing AppArmor profile")?;

        if status.success() {
            tracing::info!(profile = profile_path, "AppArmor profile enforced");
        } else {
            tracing::warn!(
                profile = profile_path,
                "failed to enforce AppArmor profile, continuing"
            );
        }
    }

    Ok(())
}
