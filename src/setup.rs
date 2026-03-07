use std::path::Path;

use anyhow::{ensure, Context};
use tokio::process::Command;

use crate::config::{AppConfig, NetworkConfig};

const KERNEL_URL: &str =
    "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/x86_64/vmlinux-6.1.102";
const RUNNER_VERSION: &str = "2.332.0";
const DEFAULT_CLOUD_IMG_URL: &str =
    "https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-amd64.img";

/// Ensures all VM prerequisites are in place: KVM, kernel, rootfs, network, and AppArmor.
pub async fn ensure_vm_assets(config: &mut AppConfig) -> anyhow::Result<()> {
    preflight_kvm()?;
    resolve_allowed_networks(&mut config.network, &config.github.token).await?;
    ensure_kernel(&config.firecracker.kernel_path).await?;
    let cloud_img_url = config.firecracker.cloud_img_url.as_deref().unwrap_or(DEFAULT_CLOUD_IMG_URL);
    ensure_golden_rootfs(&config.firecracker.rootfs_golden, cloud_img_url, &config.network).await?;
    ensure_network(&config.network).await?;
    ensure_apparmor(&config.firecracker.binary_path).await?;
    Ok(())
}

/// Resolves the `allowed_networks` list by expanding the "github" keyword
/// into actual CIDRs from https://api.github.com/meta.
async fn resolve_allowed_networks(network: &mut NetworkConfig, token: &secrecy::SecretString) -> anyhow::Result<()> {
    if network.allowed_networks.is_empty() {
        tracing::info!("no allowed_networks configured, all outbound traffic permitted");
        return Ok(());
    }

    let mut resolved = Vec::new();
    for entry in &network.allowed_networks {
        if entry.eq_ignore_ascii_case("github") {
            tracing::info!("resolving GitHub network ranges from api.github.com/meta...");
            match fetch_github_cidrs(token).await {
                Ok(cidrs) => {
                    tracing::info!(count = cidrs.len(), "fetched GitHub Actions CIDRs");
                    resolved.extend(cidrs);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "failed to fetch GitHub CIDRs — skipping network allowlist, all outbound traffic will be permitted"
                    );
                    network.resolved_networks = Vec::new();
                    return Ok(());
                }
            }
        } else {
            resolved.push(entry.clone());
        }
    }

    // Always allow DNS servers
    for dns in &network.dns {
        let dns_cidr = format!("{}/32", dns);
        if !resolved.contains(&dns_cidr) {
            resolved.push(dns_cidr);
        }
    }

    tracing::info!(
        count = resolved.len(),
        "resolved allowed networks (outbound firewall will restrict to these CIDRs)"
    );
    network.resolved_networks = resolved;
    Ok(())
}

#[derive(serde::Deserialize)]
struct GitHubMeta {
    actions: Vec<String>,
    #[serde(default)]
    git: Vec<String>,
    #[serde(default)]
    api: Vec<String>,
    #[serde(default)]
    web: Vec<String>,
}

/// Fetches GitHub's published IP ranges from the /meta endpoint.
/// Returns CIDRs needed for Actions runners (actions + git + api + web).
async fn fetch_github_cidrs(token: &secrecy::SecretString) -> anyhow::Result<Vec<String>> {
    use secrecy::ExposeSecret;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("fc-runner/0.1")
        .build()
        .context("building HTTP client for GitHub meta")?;

    let resp = client
        .get("https://api.github.com/meta")
        .bearer_auth(token.expose_secret())
        .send()
        .await
        .context("fetching https://api.github.com/meta")?;

    let meta: GitHubMeta = resp
        .error_for_status()
        .context("GitHub meta API returned error")?
        .json()
        .await
        .context("parsing GitHub meta response")?;

    let mut cidrs = Vec::new();
    cidrs.extend(meta.actions);
    cidrs.extend(meta.git);
    cidrs.extend(meta.api);
    cidrs.extend(meta.web);

    // Deduplicate
    cidrs.sort();
    cidrs.dedup();

    Ok(cidrs)
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

    // Check /dev/kvm is accessible — Firecracker needs r/w, but the orchestrator
    // only needs to verify it exists. A read-only check is sufficient; if permissions
    // are wrong, Firecracker will fail with a clear error at VM launch time.
    match std::fs::metadata(kvm_path) {
        Ok(_) => {
            tracing::info!("/dev/kvm is accessible");
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!(
                "/dev/kvm exists but metadata check failed (permission denied). \
                 Firecracker may fail to start VMs. Ensure the service has kvm group access."
            );
        }
        Err(e) => {
            anyhow::bail!("Cannot access /dev/kvm: {}", e);
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

async fn ensure_golden_rootfs(rootfs_path: &str, cloud_img_url: &str, network: &NetworkConfig) -> anyhow::Result<()> {
    if Path::new(rootfs_path).exists() {
        tracing::info!(path = rootfs_path, "golden rootfs already exists");
        return Ok(());
    }

    let parent = Path::new(rootfs_path)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("rootfs_path has no parent dir"))?;
    tokio::fs::create_dir_all(parent).await?;
    let parent_str = parent.to_str().unwrap_or("/opt/fc-runner");

    tracing::info!(path = rootfs_path, "golden rootfs not found, building from cloud image...");

    // ── Step 1: Download cloud image (cached) ───────────────────────
    let cloud_img = format!("{}/cloud-base.img", parent_str);
    if !Path::new(&cloud_img).exists() {
        tracing::info!("downloading Ubuntu minimal cloud image...");
        let status = Command::new("curl")
            .args(["-fSL", "--proto", "=https", "--tlsv1.2", "-o", &cloud_img, cloud_img_url])
            .status()
            .await
            .context("downloading cloud image")?;
        ensure!(status.success(), "failed to download cloud image");
    } else {
        tracing::info!("using cached cloud image");
    }

    // ── Step 2: Extract ext4 partition from qcow2 ───────────────────
    tracing::info!("extracting ext4 partition from cloud image...");
    let raw_img = format!("{}/cloud-raw.img", parent_str);
    let output = Command::new("qemu-img")
        .args(["convert", "-f", "qcow2", "-O", "raw", &cloud_img, &raw_img])
        .output()
        .await
        .context("converting qcow2 to raw")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("qemu-img convert failed: {}", stderr);
    }

    // Attach with partition scanning and find the ext4 partition
    let output = Command::new("losetup")
        .args(["--find", "--show", "--partscan", &raw_img])
        .output()
        .await
        .context("losetup")?;
    ensure!(output.status.success(), "losetup failed");
    let loop_dev = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Find the ext4 partition (usually p1)
    let mut ext4_part = String::new();
    for suffix in ["p1", "p2", "p15"] {
        let part = format!("{}{}", loop_dev, suffix);
        let blkid_out = Command::new("blkid")
            .arg(&part)
            .output()
            .await;
        if let Ok(out) = blkid_out {
            if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("ext4") {
                ext4_part = part;
                break;
            }
        }
    }
    if ext4_part.is_empty() {
        let _ = Command::new("losetup").args(["-d", &loop_dev]).status().await;
        let _ = tokio::fs::remove_file(&raw_img).await;
        anyhow::bail!("no ext4 partition found in cloud image");
    }

    tracing::info!(partition = %ext4_part, "found ext4 partition, extracting...");
    let status = Command::new("dd")
        .arg(format!("if={}", ext4_part))
        .arg(format!("of={}", rootfs_path))
        .args(["bs=4M", "status=progress"])
        .status()
        .await
        .context("dd partition extraction")?;
    ensure!(status.success(), "dd partition extraction failed");

    let _ = Command::new("losetup").args(["-d", &loop_dev]).status().await;
    let _ = tokio::fs::remove_file(&raw_img).await;

    // ── Step 3: Expand to 4GB, fix filesystem, resize ───────────────
    let status = Command::new("truncate")
        .args(["-s", "4G", rootfs_path])
        .status()
        .await
        .context("truncate")?;
    ensure!(status.success(), "truncate failed");

    let _ = Command::new("e2fsck")
        .args(["-f", "-y", rootfs_path])
        .status()
        .await;

    let status = Command::new("resize2fs")
        .arg(rootfs_path)
        .status()
        .await
        .context("resize2fs")?;
    ensure!(status.success(), "resize2fs failed");

    // ── Step 4: Mount and customize ─────────────────────────────────
    let mount_dir = format!("{}.mnt", rootfs_path);
    tokio::fs::create_dir_all(&mount_dir).await?;

    let status = Command::new("mount")
        .args(["-o", "loop", rootfs_path, &mount_dir])
        .status()
        .await
        .context("mounting rootfs image")?;
    ensure!(status.success(), "mount failed");

    // Mount pseudo-filesystems for chroot
    let _ = Command::new("mount").args(["--bind", "/dev", &format!("{}/dev", mount_dir)]).status().await;
    let _ = Command::new("mount").args(["--bind", "/dev/pts", &format!("{}/dev/pts", mount_dir)]).status().await;
    let _ = Command::new("mount").args(["-t", "proc", "proc", &format!("{}/proc", mount_dir)]).status().await;
    let _ = Command::new("mount").args(["-t", "sysfs", "sys", &format!("{}/sys", mount_dir)]).status().await;

    let result = build_rootfs_contents(&mount_dir, network).await;

    // Always unmount pseudo-filesystems then rootfs
    for sub in ["dev/pts", "dev", "proc", "sys"] {
        let _ = Command::new("umount")
            .arg(format!("{}/{}", mount_dir, sub))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    let umount_status = Command::new("umount")
        .arg(&mount_dir)
        .status()
        .await
        .context("unmounting rootfs")?;

    let _ = tokio::fs::remove_dir(&mount_dir).await;

    if let Err(e) = result {
        let _ = tokio::fs::remove_file(rootfs_path).await;
        return Err(e).context("building rootfs contents");
    }
    ensure!(umount_status.success(), "umount failed");

    // ── Step 5: Shrink image ────────────────────────────────────────
    tracing::info!("shrinking rootfs image...");
    let _ = Command::new("e2fsck").args(["-f", "-y", rootfs_path]).status().await;
    let _ = Command::new("resize2fs").args(["-M", rootfs_path]).status().await;

    // Calculate final size: min blocks + 512MB headroom
    let output = Command::new("dumpe2fs")
        .args(["-h", rootfs_path])
        .output()
        .await
        .context("dumpe2fs")?;
    let dumpe2fs_out = String::from_utf8_lossy(&output.stdout);
    let block_count: u64 = dumpe2fs_out
        .lines()
        .find(|l| l.starts_with("Block count:"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let block_size: u64 = dumpe2fs_out
        .lines()
        .find(|l| l.starts_with("Block size:"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096);

    if block_count > 0 {
        let final_bytes = block_count * block_size + 512 * 1024 * 1024;
        let final_blocks = final_bytes / block_size;
        let _ = Command::new("resize2fs")
            .args([rootfs_path, &final_blocks.to_string()])
            .status()
            .await;
        let _ = Command::new("truncate")
            .args(["-s", &final_bytes.to_string(), rootfs_path])
            .status()
            .await;
    }

    tracing::info!(path = rootfs_path, "golden rootfs built");
    Ok(())
}

async fn build_rootfs_contents(mount_dir: &str, network: &NetworkConfig) -> anyhow::Result<()> {
    // ── Fix fstab for Firecracker's /dev/vda ────────────────────────
    tokio::fs::write(
        format!("{}/etc/fstab", mount_dir),
        "/dev/vda\t/\text4\tdefaults,noatime\t0\t1\n",
    )
    .await?;

    // ── Fix DNS for chroot (cloud image symlinks to systemd-resolved) ─
    tracing::info!("configuring DNS for chroot...");
    let resolv_path = format!("{}/etc/resolv.conf", mount_dir);
    let _ = tokio::fs::remove_file(&resolv_path).await;
    tokio::fs::write(&resolv_path, "nameserver 8.8.8.8\nnameserver 1.1.1.1\n").await?;

    // ── Install missing packages ────────────────────────────────────
    tracing::info!("installing runner dependencies...");
    let status = Command::new("chroot")
        .args([
            mount_dir, "bash", "-c",
            "export DEBIAN_FRONTEND=noninteractive && \
             apt-get update -q && \
             apt-get install -y --no-install-recommends \
                 curl git jq ca-certificates sudo libicu74 iproute2 systemd-resolved && \
             apt-get clean && \
             rm -rf /var/lib/apt/lists/*",
        ])
        .status()
        .await
        .context("installing packages in chroot")?;
    ensure!(status.success(), "apt-get install failed");

    // Ensure /var/tmp exists (systemd-resolved needs it for PrivateTmp namespace)
    let var_tmp = format!("{}/var/tmp", mount_dir);
    tokio::fs::create_dir_all(&var_tmp).await?;
    let _ = Command::new("chmod").args(["1777", &var_tmp]).status().await;

    // Restore systemd-resolved symlink
    let _ = tokio::fs::remove_file(&resolv_path).await;
    tokio::fs::symlink("/run/systemd/resolve/stub-resolv.conf", &resolv_path).await?;

    // ── Network configuration ───────────────────────────────────────
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

    let _ = Command::new("chroot")
        .args([mount_dir, "systemctl", "enable", "systemd-networkd", "systemd-resolved"])
        .status()
        .await;

    // Belt-and-suspenders: create symlinks manually in case chroot systemctl fails
    let wants_dir = format!("{}/etc/systemd/system/multi-user.target.wants", mount_dir);
    tokio::fs::create_dir_all(&wants_dir).await?;
    for svc in ["systemd-networkd", "systemd-resolved"] {
        let symlink = format!("{}/{}.service", wants_dir, svc);
        let target = format!("/lib/systemd/system/{}.service", svc);
        if !Path::new(&symlink).exists() {
            let _ = tokio::fs::symlink(&target, &symlink).await;
        }
    }

    // Disable services that slow boot and aren't needed in ephemeral VMs
    let _ = Command::new("chroot")
        .args([
            mount_dir, "bash", "-c",
            "systemctl disable apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true; \
             systemctl disable motd-news.timer 2>/dev/null || true; \
             systemctl mask systemd-timesyncd.service 2>/dev/null || true",
        ])
        .status()
        .await;

    // ── Create runner user ──────────────────────────────────────────
    let _ = Command::new("chroot")
        .args([mount_dir, "useradd", "-m", "-s", "/bin/bash", "runner"])
        .status()
        .await;

    tokio::fs::write(
        format!("{}/etc/sudoers.d/runner", mount_dir),
        "runner ALL=(ALL) NOPASSWD:ALL\n",
    )
    .await?;

    // ── Install GitHub Actions runner ───────────────────────────────
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

    // Fix ownership
    let status = Command::new("chroot")
        .args([mount_dir, "chown", "-R", "runner:runner", "/home/runner"])
        .status()
        .await?;
    ensure!(status.success(), "chown failed");

    // Write entrypoint (supports JIT and registration modes)
    tokio::fs::write(
        format!("{}/entrypoint.sh", mount_dir),
        r#"#!/bin/bash
set -euo pipefail
exec > /var/log/runner.log 2>&1

echo "=== fc-runner entrypoint $(date) ==="

if [ ! -f /etc/fc-runner-env ]; then
    echo "ERROR: /etc/fc-runner-env not found"
    sleep 3
    poweroff -f
fi

source /etc/fc-runner-env
echo "VM_ID=${VM_ID} MODE=${RUNNER_MODE:-jit}"

# Wait for network connectivity
for i in $(seq 1 30); do
    if ip route show default | grep -q default 2>/dev/null; then
        if curl -sf --connect-timeout 3 --max-time 5 https://github.com > /dev/null 2>&1; then
            echo "Network ready"
            break
        fi
    fi
    echo "Waiting for network ($i/30)..."
    sleep 1
done

cd /home/runner

if [ "${RUNNER_MODE:-jit}" = "jit" ]; then
    echo "Starting runner (JIT mode)..."
    sudo -u runner ./run.sh --jitconfig "${RUNNER_TOKEN}"
else
    echo "Registering runner..."
    sudo -u runner ./config.sh \
        --url "${REPO_URL}" \
        --token "${RUNNER_TOKEN}" \
        --name "${RUNNER_NAME:-fc-$(hostname)}" \
        --labels "firecracker,linux,x64" \
        --ephemeral \
        --unattended \
        --disableupdate \
        --work /home/runner/_work
    echo "Starting runner (registered mode)..."
    sudo -u runner ./run.sh
fi

echo "Runner finished, shutting down"
poweroff -f
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

    // Create rc-local.service unit (not shipped by default in Ubuntu 24.04 cloud images)
    let rc_local_unit = format!("{}/etc/systemd/system/rc-local.service", mount_dir);
    if !Path::new(&rc_local_unit).exists() {
        tokio::fs::write(
            &rc_local_unit,
            "[Unit]\n\
             Description=/etc/rc.local Compatibility\n\
             ConditionFileIsExecutable=/etc/rc.local\n\
             \n\
             [Service]\n\
             Type=forking\n\
             ExecStart=/etc/rc.local\n\
             TimeoutSec=0\n\
             RemainAfterExit=yes\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
        )
        .await?;
    }

    // Enable rc-local.service so the entrypoint runs on boot
    let _ = Command::new("chroot")
        .args([mount_dir, "systemctl", "enable", "rc-local.service"])
        .status()
        .await;

    // Belt-and-suspenders: create the symlink manually in case systemctl
    // behaves oddly inside a plain chroot
    let wants_dir = format!(
        "{}/etc/systemd/system/multi-user.target.wants",
        mount_dir
    );
    tokio::fs::create_dir_all(&wants_dir).await?;
    let symlink_path = format!("{}/rc-local.service", wants_dir);
    if !Path::new(&symlink_path).exists() {
        let _ = tokio::fs::symlink(
            "/etc/systemd/system/rc-local.service",
            &symlink_path,
        )
        .await;
    }

    Ok(())
}

/// Ensures IP forwarding is enabled and iptables NAT rules are in place.
/// TAP devices are now created per-VM in firecracker.rs, not at startup.
async fn ensure_network(network: &NetworkConfig) -> anyhow::Result<()> {
    cleanup_stale_taps().await;
    ensure_ip_forwarding().await?;
    ensure_nat_rules(network).await?;
    Ok(())
}

/// Clean up any TAP devices left over from a previous crash.
async fn cleanup_stale_taps() {
    for i in 0..16 {
        let tap_name = format!("tap-fc{}", i);
        let status = Command::new("ip")
            .args(["link", "show", &tap_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if status.map(|s| s.success()).unwrap_or(false) {
            tracing::info!(tap = %tap_name, "cleaning up stale TAP device from previous run");
            let _ = Command::new("ip")
                .args(["link", "delete", &tap_name])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        }
    }
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

/// VM subnet covering all per-VM TAP devices (172.16.0.0/16).
const VM_SUBNET: &str = "172.16.0.0/16";

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

    tracing::info!(iface = %default_iface, subnet = VM_SUBNET, "configuring subnet-based NAT rules");

    // MASQUERADE — outbound NAT for all VM subnets
    add_iptables_rule_if_missing(&[
        "-t", "nat", "-A", "POSTROUTING",
        "-s", VM_SUBNET,
        "-o", &default_iface,
        "-j", "MASQUERADE",
    ], &[
        "-t", "nat", "-C", "POSTROUTING",
        "-s", VM_SUBNET,
        "-o", &default_iface,
        "-j", "MASQUERADE",
    ]).await?;

    // TCP MSS clamping — prevents large-download stalls caused by PMTU black holes.
    // Adjusts SYN packet MSS to fit the outgoing interface MTU.
    add_iptables_rule_if_missing(&[
        "-t", "mangle", "-A", "FORWARD",
        "-p", "tcp", "--tcp-flags", "SYN,RST", "SYN",
        "-s", VM_SUBNET,
        "-j", "TCPMSS", "--clamp-mss-to-pmtu",
    ], &[
        "-t", "mangle", "-C", "FORWARD",
        "-p", "tcp", "--tcp-flags", "SYN,RST", "SYN",
        "-s", VM_SUBNET,
        "-j", "TCPMSS", "--clamp-mss-to-pmtu",
    ]).await?;

    // FORWARD — allow established return traffic to VM subnets.
    // Insert at top of chain (-I FORWARD 1) so it runs BEFORE UFW/Docker rules
    // which would otherwise DROP the return traffic.
    add_iptables_rule_if_missing(&[
        "-I", "FORWARD", "1",
        "-d", VM_SUBNET,
        "-m", "state", "--state", "RELATED,ESTABLISHED",
        "-j", "ACCEPT",
    ], &[
        "-C", "FORWARD",
        "-d", VM_SUBNET,
        "-m", "state", "--state", "RELATED,ESTABLISHED",
        "-j", "ACCEPT",
    ]).await?;

    if network.resolved_networks.is_empty() {
        // No allowlist — allow all outbound traffic from VM subnets
        add_iptables_rule_if_missing(&[
            "-I", "FORWARD", "2",
            "-s", VM_SUBNET,
            "-o", &default_iface,
            "-j", "ACCEPT",
        ], &[
            "-C", "FORWARD",
            "-s", VM_SUBNET,
            "-o", &default_iface,
            "-j", "ACCEPT",
        ]).await?;
    } else {
        // Allowlist mode — use ipset for efficient matching (thousands of CIDRs)
        tracing::info!(
            count = network.resolved_networks.len(),
            "applying network allowlist via ipset"
        );

        // Create ipset if it doesn't exist, then flush it
        let _ = Command::new("ipset")
            .args(["create", "fc-allowed", "hash:net", "family", "inet", "hashsize", "16384", "maxelem", "65536", "-exist"])
            .status()
            .await
            .context("creating ipset fc-allowed")?;

        let _ = Command::new("ipset")
            .args(["flush", "fc-allowed"])
            .status()
            .await
            .context("flushing ipset fc-allowed")?;

        // Add all IPv4 CIDRs to the set (skip IPv6 — NAT is IPv4 only)
        for cidr in &network.resolved_networks {
            if cidr.contains(':') {
                continue; // Skip IPv6
            }
            let status = Command::new("ipset")
                .args(["add", "fc-allowed", cidr, "-exist"])
                .status()
                .await
                .context("adding CIDR to ipset")?;
            if !status.success() {
                tracing::warn!(cidr = %cidr, "failed to add CIDR to ipset, skipping");
            }
        }

        // Insert FORWARD rules at top of chain (after RELATED,ESTABLISHED)
        // so they run BEFORE UFW/Docker chains
        add_iptables_rule_if_missing(&[
            "-I", "FORWARD", "2",
            "-s", VM_SUBNET,
            "-o", &default_iface,
            "-m", "set", "--match-set", "fc-allowed", "dst",
            "-j", "ACCEPT",
        ], &[
            "-C", "FORWARD",
            "-s", VM_SUBNET,
            "-o", &default_iface,
            "-m", "set", "--match-set", "fc-allowed", "dst",
            "-j", "ACCEPT",
        ]).await?;

        // Drop all other outbound traffic from VM subnets
        add_iptables_rule_if_missing(&[
            "-I", "FORWARD", "3",
            "-s", VM_SUBNET,
            "-o", &default_iface,
            "-j", "DROP",
        ], &[
            "-C", "FORWARD",
            "-s", VM_SUBNET,
            "-o", &default_iface,
            "-j", "DROP",
        ]).await?;

        tracing::info!("network allowlist applied via ipset — unmatched outbound traffic will be dropped");
    }

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
        .await
        .with_context(|| format!("running iptables {:?}", check_args))?;

    if status.success() {
        return Ok(());
    }

    // Add the rule
    let status = Command::new("iptables")
        .args(add_args)
        .status()
        .await
        .with_context(|| format!("running iptables {:?}", add_args))?;
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
