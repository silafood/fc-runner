use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{ensure, Context};
use tokio::process::Command;

use crate::config::{AppConfig, NetworkConfig};

/// Download a file via reqwest (replaces curl).
async fn download_file(url: &str, dest: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let response = reqwest::get(url)
        .await
        .context("HTTP request failed")?
        .error_for_status()
        .context("HTTP error response")?;

    let bytes = response.bytes().await.context("reading response body")?;
    let mut file = tokio::fs::File::create(dest).await.context("creating output file")?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    Ok(())
}

/// Mount an ext4 image via loop device.
///
/// Uses `mount` command because the kernel mount(2) syscall doesn't handle
/// loop device setup — that's done in userspace by the mount binary.
async fn mount_ext4(image: &str, target: &str, readonly: bool) -> anyhow::Result<()> {
    let opts = if readonly { "loop,ro,noload,noatime" } else { "loop,noatime" };
    let status = Command::new("mount")
        .args(["-o", opts, image, target])
        .status()
        .await
        .context("running mount")?;
    ensure!(status.success(), "mount -o {} {} {} failed", opts, image, target);
    Ok(())
}
/// Bind mount a directory.
async fn bind_mount(source: &str, target: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        nix::mount::mount(Some(source), target, None::<&str>, nix::mount::MsFlags::MS_BIND, None::<&str>)
            .map_err(|e| anyhow::anyhow!("bind mount failed: {}", e))?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let status = Command::new("mount").args(["--bind", source, target]).status().await?;
        ensure!(status.success(), "bind mount failed");
        Ok(())
    }
}

/// Mount a pseudo-filesystem (proc, sysfs).
async fn mount_pseudo(fstype: &str, target: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        nix::mount::mount(None::<&str>, target, Some(fstype), nix::mount::MsFlags::empty(), None::<&str>)
            .map_err(|e| anyhow::anyhow!("mount {} failed: {}", fstype, e))?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let status = Command::new("mount").args(["-t", fstype, fstype, target]).status().await?;
        ensure!(status.success(), "mount {} failed", fstype);
        Ok(())
    }
}

/// Lazy unmount a filesystem.
async fn lazy_umount(target: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        nix::mount::umount2(target, nix::mount::MntFlags::MNT_DETACH)
            .map_err(|e| anyhow::anyhow!("umount failed: {}", e))?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let status = Command::new("umount").args(["-l", target])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status().await?;
        ensure!(status.success(), "umount failed");
        Ok(())
    }
}

/// Run a command inside a chroot.
/// Uses the external `chroot` binary — the chroot(2) syscall requires
/// CAP_SYS_CHROOT.
fn chroot_command(root: &str, program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new("chroot");
    cmd.arg(root).arg(program).args(args);
    cmd
}

fn set_executable(path: &str) -> anyhow::Result<()> {
    let meta = std::fs::metadata(path)?;
    let mut perms = meta.permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

const KERNEL_URL: &str =
    "https://github.com/silafood/fc-runner/releases/download/v0.1.0/vmlinux-6.1.102";
const RUNNER_VERSION: &str = "2.332.0";
const DEFAULT_CLOUD_IMG_URL: &str =
    "https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-amd64.img";

/// Ensures all VM prerequisites are in place: KVM, kernel, rootfs, and network.
pub async fn ensure_vm_assets(config: &mut AppConfig) -> anyhow::Result<()> {
    preflight_kvm()?;
    resolve_allowed_networks(&mut config.network, config.github.token.as_ref()).await?;
    ensure_kernel(&config.firecracker.kernel_path).await?;
    let cloud_img_url = config.firecracker.cloud_img_url.as_deref().unwrap_or(DEFAULT_CLOUD_IMG_URL);
    ensure_golden_rootfs(&config.firecracker.rootfs_golden, cloud_img_url, &config.network).await?;
    ensure_network(&config.network).await?;
    ensure_directories(config).await?;
    Ok(())
}

/// Create runtime directories (work_dir, jailer chroot base) if they don't exist.
async fn ensure_directories(config: &AppConfig) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&config.runner.work_dir).await
        .with_context(|| format!("creating work_dir: {}", config.runner.work_dir))?;

    if config.firecracker.jailer_path.is_some() {
        let base = &config.firecracker.jailer_chroot_base;
        tokio::fs::create_dir_all(base).await
            .with_context(|| format!("creating jailer_chroot_base: {}", base))?;
        tracing::info!(path = %base, "jailer chroot base directory ready");
    }

    Ok(())
}

/// Resolves the `allowed_networks` list by expanding the "github" keyword
/// into actual CIDRs from https://api.github.com/meta.
async fn resolve_allowed_networks(network: &mut NetworkConfig, token: Option<&secrecy::SecretString>) -> anyhow::Result<()> {
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
async fn fetch_github_cidrs(token: Option<&secrecy::SecretString>) -> anyhow::Result<Vec<String>> {
    use secrecy::ExposeSecret;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("fc-runner/0.1")
        .build()
        .context("building HTTP client for GitHub meta")?;

    let mut req = client.get("https://api.github.com/meta");
    if let Some(token) = token {
        req = req.bearer_auth(token.expose_secret());
    }
    let resp = req
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

    download_file(KERNEL_URL, kernel_path).await?;

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
        download_file(cloud_img_url, &cloud_img).await?;
    } else {
        tracing::info!("using cached cloud image");
    }

    // ── Step 2: Convert qcow2 to raw (pure Rust via qcow2-rs) ──────
    tracing::info!("converting qcow2 to raw image...");
    let raw_img = format!("{}/cloud-raw.img", parent_str);
    {
        use qcow2_rs::dev::Qcow2DevParams;
        use qcow2_rs::utils::qcow2_setup_dev_tokio;

        let path = std::path::PathBuf::from(&cloud_img);
        let params = Qcow2DevParams::new(9, None, None, true, false);
        let dev = qcow2_setup_dev_tokio(&path, &params)
            .await
            .map_err(|e| anyhow::anyhow!("failed to open qcow2: {:?}", e))?;

        let virtual_size = dev.info.virtual_size();
        tracing::info!(virtual_size_mb = virtual_size / (1024 * 1024), "qcow2 virtual disk size");

        let mut output = tokio::fs::File::create(&raw_img).await
            .context("creating raw image")?;

        let buf_size: usize = 1024 * 1024; // 1 MiB (must be cluster-aligned)
        let mut buf = vec![0u8; buf_size];
        let mut offset: u64 = 0;

        while offset < virtual_size {
            let to_read = std::cmp::min(buf_size as u64, virtual_size - offset) as usize;
            buf[..to_read].fill(0);
            let _ = dev.read_at(&mut buf[..to_read], offset)
                .await
                .map_err(|e| anyhow::anyhow!("qcow2 read at offset {}: {:?}", offset, e))?;
            tokio::io::AsyncWriteExt::write_all(&mut output, &buf[..to_read]).await?;
            offset += to_read as u64;
        }

        tokio::io::AsyncWriteExt::flush(&mut output).await?;
        tracing::info!(bytes = offset, "qcow2 → raw conversion complete");
    }

    // Find ext4 partition and extract it (pure Rust — no losetup/blkid/dd)
    {
        let raw_img_clone = raw_img.clone();
        let rootfs_out = rootfs_path.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            use std::io::{Read, Seek, SeekFrom, Write};

            let mut img = std::fs::File::open(&raw_img_clone)
                .context("opening raw image")?;

            // Parse partition table (GPT or MBR)
            let partitions = bootsector::list_partitions(&mut img, &bootsector::Options::default())
                .map_err(|e| anyhow::anyhow!("failed to parse partition table: {:?}", e))?;

            tracing::info!(count = partitions.len(), "found partitions in raw image");

            // Find the ext4 partition by checking superblock magic (0xEF53 at offset 1080)
            let mut ext4_offset: Option<u64> = None;
            let mut ext4_len: Option<u64> = None;
            for part in &partitions {
                tracing::info!(
                    id = %part.id,
                    first_byte = part.first_byte,
                    len = part.len,
                    "checking partition for ext4"
                );
                // ext4 superblock is at byte 1024 within the partition,
                // magic number (0xEF53) is at offset 0x38 (56) within the superblock
                img.seek(SeekFrom::Start(part.first_byte + 1024 + 56))?;
                let mut magic = [0u8; 2];
                if img.read_exact(&mut magic).is_ok() && u16::from_le_bytes(magic) == 0xEF53 {
                    tracing::info!(id = %part.id, offset = part.first_byte, "found ext4 partition");
                    ext4_offset = Some(part.first_byte);
                    ext4_len = Some(part.len);
                    break;
                }
            }

            // Also check if the whole image is raw ext4 (no partition table match)
            if ext4_offset.is_none() {
                img.seek(SeekFrom::Start(1024 + 56))?;
                let mut magic = [0u8; 2];
                if img.read_exact(&mut magic).is_ok() && u16::from_le_bytes(magic) == 0xEF53 {
                    let file_len = img.metadata()?.len();
                    tracing::info!("raw image is ext4 (no partition table)");
                    ext4_offset = Some(0);
                    ext4_len = Some(file_len);
                }
            }

            let offset = ext4_offset
                .ok_or_else(|| anyhow::anyhow!("no ext4 partition found in cloud image"))?;
            let len = ext4_len.unwrap();

            // Extract partition to rootfs file
            tracing::info!(offset, len, "extracting ext4 partition...");
            img.seek(SeekFrom::Start(offset))?;
            let mut output = std::fs::File::create(&rootfs_out)
                .context("creating rootfs file")?;

            let buf_size: usize = 4 * 1024 * 1024; // 4 MiB
            let mut buf = vec![0u8; buf_size];
            let mut remaining = len;
            while remaining > 0 {
                let to_read = std::cmp::min(buf_size as u64, remaining) as usize;
                let n = img.read(&mut buf[..to_read])?;
                if n == 0 { break; }
                output.write_all(&buf[..n])?;
                remaining -= n as u64;
            }
            output.flush()?;
            tracing::info!("ext4 partition extracted");

            // Expand to 6GB (need space for cargo, build tools, etc.)
            let file = std::fs::OpenOptions::new().write(true).open(&rootfs_out)?;
            file.set_len(6 * 1024 * 1024 * 1024)?;
            tracing::info!("rootfs expanded to 6 GiB");

            Ok(())
        })
        .await
        .context("partition extraction task panicked")??;
    }
    let _ = tokio::fs::remove_file(&raw_img).await;

    // Run e2fsck + resize2fs to expand the filesystem to fill the 4GB image.
    // These may fail if not installed — that's OK, the original partition
    // (~2GB) has enough room for the runner install. We log a warning and continue.
    match run_e2fs_tool("e2fsck", &["-f", "-y", rootfs_path]).await {
        Ok(s) if s.success() => tracing::info!("e2fsck passed"),
        Ok(s) => tracing::warn!("e2fsck exited with {}", s),
        Err(e) => tracing::warn!("e2fsck skipped (not found): {}", e),
    }
    match run_e2fs_tool("resize2fs", &[rootfs_path]).await {
        Ok(s) if s.success() => tracing::info!("resize2fs expanded filesystem to fill image"),
        Ok(s) => tracing::warn!("resize2fs exited with {}", s),
        Err(e) => tracing::warn!("resize2fs skipped (not found): {}", e),
    }

    // ── Step 4: Mount and customize ─────────────────────────────────
    let mount_dir = format!("{}.mnt", rootfs_path);
    tokio::fs::create_dir_all(&mount_dir).await?;

    mount_ext4(rootfs_path, &mount_dir, false).await
        .context("mounting rootfs image")?;

    // Mount pseudo-filesystems for chroot
    let _ = bind_mount("/dev", &format!("{}/dev", mount_dir)).await;
    let _ = bind_mount("/dev/pts", &format!("{}/dev/pts", mount_dir)).await;
    let _ = mount_pseudo("proc", &format!("{}/proc", mount_dir)).await;
    let _ = mount_pseudo("sysfs", &format!("{}/sys", mount_dir)).await;

    let result = build_rootfs_contents(&mount_dir, network).await;

    // Always unmount pseudo-filesystems then rootfs
    for sub in ["dev/pts", "dev", "proc", "sys"] {
        let _ = lazy_umount(&format!("{}/{}", mount_dir, sub)).await;
    }
    let umount_result = lazy_umount(&mount_dir).await;

    let _ = tokio::fs::remove_dir(&mount_dir).await;

    if let Err(e) = result {
        let _ = tokio::fs::remove_file(rootfs_path).await;
        return Err(e).context("building rootfs contents");
    }
    umount_result.context("unmounting rootfs")?;

    // ── Step 5: Shrink image ────────────────────────────────────────
    tracing::info!("shrinking rootfs image...");
    let _ = run_e2fs_tool("e2fsck", &["-f", "-y", rootfs_path]).await;
    let _ = run_e2fs_tool("resize2fs", &["-M", rootfs_path]).await;

    // Read block count + block size directly from ext4 superblock (replaces dumpe2fs)
    let (block_count, block_size) = read_ext4_superblock(rootfs_path)
        .context("reading ext4 superblock")?;

    if block_count > 0 {
        let final_bytes = block_count * block_size + 512 * 1024 * 1024;
        let final_blocks = final_bytes / block_size;
        let _ = run_e2fs_tool("resize2fs", &[rootfs_path, &final_blocks.to_string()]).await;
        let f = std::fs::OpenOptions::new().write(true).open(rootfs_path)?;
        f.set_len(final_bytes)?;
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

    // ── Install packages (runner deps + build tools) ──────────────
    tracing::info!("installing runner dependencies and build tools...");
    let status = chroot_command(mount_dir, "bash", &["-c",
            "export DEBIAN_FRONTEND=noninteractive && \
             apt-get update -q && \
             apt-get install -y --no-install-recommends \
                 curl git jq ca-certificates sudo libicu74 iproute2 systemd-resolved \
                 build-essential pkg-config libssl-dev \
                 gcc g++ make cmake \
                 python3 python3-pip python3-venv \
                 nodejs npm \
                 docker.io containerd \
                 wget tar gzip xz-utils \
                 zip bzip2 \
                 libffi-dev zlib1g-dev \
                 net-tools dnsutils iputils-ping \
                 locales && \
             apt-get clean && \
             rm -rf /var/lib/apt/lists/*"])
        .status()
        .await
        .context("installing packages in chroot")?;
    ensure!(status.success(), "apt-get install failed");

    // ── Install Rust toolchain ──────────────────────────────────────
    tracing::info!("installing Rust toolchain...");
    let status = chroot_command(mount_dir, "bash", &["-c",
            "su - runner -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable' && \
             ln -sf /home/runner/.cargo/bin/cargo /usr/local/bin/cargo && \
             ln -sf /home/runner/.cargo/bin/rustc /usr/local/bin/rustc && \
             ln -sf /home/runner/.cargo/bin/rustup /usr/local/bin/rustup && \
             echo 'export PATH=/home/runner/.cargo/bin:$PATH' >> /home/runner/.bashrc && \
             echo 'PATH=/home/runner/.cargo/bin:/usr/local/bin:/usr/bin:/bin' > /etc/environment"])
        .status()
        .await
        .context("installing Rust toolchain in chroot")?;
    if !status.success() {
        tracing::warn!("Rust toolchain installation failed — CI jobs needing cargo will not work");
    }

    // Ensure /var/tmp exists (systemd-resolved needs it for PrivateTmp namespace)
    let var_tmp = format!("{}/var/tmp", mount_dir);
    tokio::fs::create_dir_all(&var_tmp).await?;
    std::fs::set_permissions(&var_tmp, std::os::unix::fs::PermissionsExt::from_mode(0o1777))?;

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

    let _ = chroot_command(mount_dir, "systemctl", &["enable", "systemd-networkd", "systemd-resolved"])
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
    // Mask EFI/boot mount units — cloud image ships these but Firecracker has no
    // EFI partition, causing emergency mode on boot
    let _ = chroot_command(mount_dir, "bash", &["-c",
            "systemctl disable apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true; \
             systemctl disable motd-news.timer 2>/dev/null || true; \
             systemctl mask systemd-timesyncd.service 2>/dev/null || true; \
             systemctl mask boot-efi.mount 2>/dev/null || true; \
             systemctl mask systemd-gpt-auto-generator 2>/dev/null || true"])
        .status()
        .await;

    // Remove any cloud-image fstab entries beyond root (EFI, swap, BOOT label, etc.)
    // Our fstab was already written above with just /dev/vda → /
    // Also remove /etc/fstab.d/ snippets if present
    let fstab_d = format!("{}/etc/fstab.d", mount_dir);
    if Path::new(&fstab_d).exists() {
        let _ = tokio::fs::remove_dir_all(&fstab_d).await;
    }

    // ── Create runner user ──────────────────────────────────────────
    let _ = chroot_command(mount_dir, "useradd", &["-m", "-s", "/bin/bash", "runner"])
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
    download_file(&runner_url, &runner_tarball).await?;

    // Extract tar.gz in pure Rust
    {
        let tarball_path = runner_tarball.clone();
        let extract_dir = runner_dir.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let file = std::fs::File::open(&tarball_path)
                .context("opening runner tarball")?;
            let gz = flate2::read::GzDecoder::new(file);
            let mut archive = tar::Archive::new(gz);
            archive.unpack(&extract_dir)
                .context("extracting runner tarball")?;
            Ok(())
        })
        .await
        .context("tar extraction task panicked")??;
    }
    let _ = tokio::fs::remove_file(&runner_tarball).await;

    // Fix ownership
    let status = chroot_command(mount_dir, "chown", &["-R", "runner:runner", "/home/runner"])
        .status()
        .await?;
    ensure!(status.success(), "chown failed");

    // ── Install fc-runner binary into the rootfs ────────────────────
    // The agent reads MMDS directly, manages the runner with explicit env,
    // and reports to the host via VSOCK. No shell entrypoint needed.
    let self_exe = std::env::current_exe().context("getting current executable path")?;
    let guest_bin = format!("{}/usr/local/bin/fc-runner", mount_dir);
    tokio::fs::create_dir_all(format!("{}/usr/local/bin", mount_dir)).await?;
    tokio::fs::copy(&self_exe, &guest_bin).await
        .context("copying fc-runner binary into rootfs")?;
    set_executable(&guest_bin)?;
    tracing::info!("installed fc-runner agent binary into rootfs");

    // Write entrypoint that prefers fc-runner agent, with shell fallback.
    // Uses reboot -f (not poweroff -f) because Firecracker has no ACPI —
    // poweroff halts the CPU in a loop without triggering KVM_EXIT_SHUTDOWN.
    // reboot -f with reboot=k boot arg triggers keyboard controller reset
    // which Firecracker intercepts as KVM_EXIT_SHUTDOWN for a clean VMM exit.
    tokio::fs::write(
        format!("{}/entrypoint.sh", mount_dir),
        r#"#!/bin/bash
set -euo pipefail
exec > /var/log/runner.log 2>&1

echo "=== fc-runner entrypoint $(date) ==="

# Prefer the Rust agent (reads MMDS directly, explicit env, VSOCK reporting)
if [ -x /usr/local/bin/fc-runner ]; then
    echo "Using fc-runner agent"
    exec /usr/local/bin/fc-runner agent --log-level info
fi

# Fallback: shell-based entrypoint (legacy)
echo "fc-runner binary not found, using shell fallback"

if [ ! -f /etc/fc-runner-env ]; then
    echo "ERROR: /etc/fc-runner-env not found"
    sleep 3
    reboot -f
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

# Source Rust environment for CI jobs
export PATH="/home/runner/.cargo/bin:$PATH"

if [ "${RUNNER_MODE:-jit}" = "jit" ]; then
    echo "Starting runner (JIT mode)..."
    sudo -E -u runner ./run.sh --jitconfig "${RUNNER_TOKEN}"
else
    echo "Registering runner..."
    sudo -E -u runner ./config.sh \
        --url "${REPO_URL}" \
        --token "${RUNNER_TOKEN}" \
        --name "${RUNNER_NAME:-fc-$(hostname)}" \
        --labels "firecracker,linux,x64" \
        --ephemeral \
        --unattended \
        --disableupdate \
        --work /home/runner/_work
    echo "Starting runner (registered mode)..."
    sudo -E -u runner ./run.sh
fi

echo "Runner finished, shutting down"
reboot -f
"#,
    )
    .await?;

    set_executable(&format!("{}/entrypoint.sh", mount_dir))?;

    // rc.local to run entrypoint on boot
    tokio::fs::write(
        format!("{}/etc/rc.local", mount_dir),
        "#!/bin/bash\n/entrypoint.sh >> /var/log/runner.log 2>&1 &\nexit 0\n",
    )
    .await?;

    set_executable(&format!("{}/etc/rc.local", mount_dir))?;

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
    let _ = chroot_command(mount_dir, "systemctl", &["enable", "rc-local.service"])
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
        if crate::netlink::link_exists(&tap_name).await {
            tracing::info!(tap = %tap_name, "cleaning up stale TAP device from previous run");
            let _ = crate::netlink::delete_link(&tap_name).await;
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
    tokio::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
        .await
        .context("writing to /proc/sys/net/ipv4/ip_forward")?;
    Ok(())
}

/// VM subnet covering all per-VM TAP devices (172.16.0.0/16).
const VM_SUBNET: &str = "172.16.0.0/16";

async fn ensure_nat_rules(network: &NetworkConfig) -> anyhow::Result<()> {
    // Find default route interface from /proc/net/route
    let route_table = tokio::fs::read_to_string("/proc/net/route")
        .await
        .context("reading /proc/net/route")?;
    let default_iface = route_table
        .lines()
        .skip(1) // skip header
        .find(|l| {
            let mut cols = l.split_whitespace();
            // Destination (col 1) == "00000000" means default route
            cols.nth(1).map(|d| d == "00000000").unwrap_or(false)
        })
        .and_then(|l| l.split_whitespace().next())
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

/// Run an e2fsprogs tool (e2fsck, resize2fs) using absolute path with fallback.
///
/// Systemd services often have a restricted PATH that doesn't include /sbin.
/// Try /sbin/ first, then /usr/sbin/, then bare name as fallback.
async fn run_e2fs_tool(tool: &str, args: &[&str]) -> anyhow::Result<std::process::ExitStatus> {
    for prefix in ["/sbin/", "/usr/sbin/", ""] {
        let bin = format!("{}{}", prefix, tool);
        match Command::new(&bin).args(args).status().await {
            Ok(status) => return Ok(status),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("running {}", bin)),
        }
    }
    anyhow::bail!("{} not found in /sbin, /usr/sbin, or PATH", tool)
}

/// Read block_count and block_size from the ext4 superblock (replaces dumpe2fs).
///
/// The ext4 superblock starts at byte offset 1024 in the filesystem image.
/// - block_size = 2^(10 + s_log_block_size)  [offset 24, 4 bytes LE]
/// - block_count = s_blocks_count_lo [offset 4, 4 bytes LE]
///   + (s_blocks_count_hi << 32) [offset 336, 4 bytes LE] (64-bit ext4)
fn read_ext4_superblock(path: &str) -> anyhow::Result<(u64, u64)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {} for superblock read", path))?;

    // Superblock starts at offset 1024
    f.seek(SeekFrom::Start(1024))?;
    let mut sb = [0u8; 512];
    f.read_exact(&mut sb)?;

    // Verify ext4 magic at offset 56: 0xEF53
    let magic = u16::from_le_bytes([sb[56], sb[57]]);
    ensure!(magic == 0xEF53, "not a valid ext4 image (magic: {:#06x})", magic);

    // s_blocks_count_lo at offset 4 (4 bytes)
    let blocks_lo = u32::from_le_bytes([sb[4], sb[5], sb[6], sb[7]]) as u64;
    // s_blocks_count_hi at offset 336 (4 bytes) — present in 64-bit ext4
    let blocks_hi = u32::from_le_bytes([sb[336], sb[337], sb[338], sb[339]]) as u64;
    let block_count = blocks_lo | (blocks_hi << 32);

    // s_log_block_size at offset 24 (4 bytes)
    let log_block_size = u32::from_le_bytes([sb[24], sb[25], sb[26], sb[27]]);
    let block_size = 1u64 << (10 + log_block_size);

    tracing::info!(
        block_count = block_count,
        block_size = block_size,
        total_size_mb = (block_count * block_size) / (1024 * 1024),
        "read ext4 superblock"
    );

    Ok((block_count, block_size))
}
