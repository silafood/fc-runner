//! OCI image pull and convert to ext4 for Firecracker.
//!
//! Pulls an OCI container image from a registry (Docker Hub, GHCR, etc.),
//! extracts all layers onto a loop-mounted ext4 image, and handles OCI
//! whiteout files. The resulting ext4 image is used as the golden rootfs.

use std::path::Path;

use anyhow::{Context, ensure};
use oci_client::manifest::{IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_GZIP_MEDIA_TYPE};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use tokio::process::Command;

/// Default rootfs size in bytes (6 GiB).
const DEFAULT_IMAGE_SIZE: u64 = 6 * 1024 * 1024 * 1024;

/// Pull an OCI image and convert it to an ext4 rootfs.
///
/// If the image has already been pulled and the digest matches, the cached
/// rootfs is reused. The `fc-runner` binary is installed into the image
/// automatically if not already present.
pub async fn pull_and_convert(image_ref: &str, output_path: &str) -> anyhow::Result<()> {
    let reference: Reference = image_ref.parse().context("parsing OCI image reference")?;

    let client = Client::default();
    let auth = RegistryAuth::Anonymous;

    // Pull manifest to get digest and layer info
    let (manifest, digest) = client
        .pull_image_manifest(&reference, &auth)
        .await
        .context("pulling image manifest")?;

    // Check cache — skip pull if digest matches
    let digest_file = format!("{}.digest", output_path);
    if Path::new(output_path).exists() && Path::new(&digest_file).exists() {
        let cached = tokio::fs::read_to_string(&digest_file)
            .await
            .unwrap_or_default();
        if cached.trim() == digest {
            tracing::info!(
                image = image_ref,
                "OCI image unchanged (digest match), using cached rootfs"
            );
            return Ok(());
        }
    }

    tracing::info!(
        image = image_ref,
        digest = %digest,
        layers = manifest.layers.len(),
        "pulling OCI image and converting to ext4"
    );

    // Remove stale rootfs if exists
    let _ = tokio::fs::remove_file(output_path).await;

    // Create blank ext4 image
    create_ext4_image(output_path, DEFAULT_IMAGE_SIZE).await?;

    // Mount the ext4 image
    let mount_dir = format!("{}-oci-mount", output_path);
    tokio::fs::create_dir_all(&mount_dir).await?;
    mount_ext4(output_path, &mount_dir).await?;

    // Extract layers in order (bottom to top)
    let result = extract_all_layers(&client, &reference, &auth, &manifest, &mount_dir).await;

    // Always unmount, even on error
    let umount_result = umount(&mount_dir).await;
    let _ = tokio::fs::remove_dir(&mount_dir).await;

    // Propagate errors
    result.context("extracting OCI layers")?;
    umount_result.context("unmounting OCI image")?;

    // Install fc-runner agent if not already in the image
    install_agent_if_missing(output_path).await?;

    // Save digest for cache check
    tokio::fs::write(&digest_file, &digest).await?;

    tracing::info!(
        image = image_ref,
        path = output_path,
        "OCI image converted to ext4 rootfs"
    );
    Ok(())
}

/// Extract all layers from the manifest onto the mounted rootfs.
async fn extract_all_layers(
    client: &Client,
    reference: &Reference,
    _auth: &RegistryAuth,
    manifest: &oci_client::manifest::OciImageManifest,
    mount_dir: &str,
) -> anyhow::Result<()> {
    for (i, layer) in manifest.layers.iter().enumerate() {
        let media_type = &layer.media_type;

        // Only extract filesystem layers (gzip compressed tar)
        if media_type != IMAGE_LAYER_GZIP_MEDIA_TYPE
            && media_type != IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE
        {
            tracing::debug!(
                layer = i + 1,
                media_type = %media_type,
                "skipping non-filesystem layer"
            );
            continue;
        }

        tracing::info!(
            layer = i + 1,
            total = manifest.layers.len(),
            size = layer.size,
            digest = %layer.digest,
            "extracting layer"
        );

        let mut layer_data: Vec<u8> = Vec::with_capacity(layer.size as usize);
        client
            .pull_blob(reference, layer, &mut layer_data)
            .await
            .with_context(|| format!("pulling layer {} ({})", i + 1, layer.digest))?;

        extract_layer(&layer_data, mount_dir)
            .with_context(|| format!("extracting layer {} ({})", i + 1, layer.digest))?;
    }
    Ok(())
}

/// Extract a single tar.gz layer onto the mount directory, handling OCI whiteouts.
fn extract_layer(layer_data: &[u8], mount_dir: &str) -> anyhow::Result<()> {
    let gz = flate2::read::GzDecoder::new(layer_data);
    let mut archive = tar::Archive::new(gz);
    archive.set_overwrite(true);
    // Preserve ownership and permissions
    archive.set_preserve_ownerships(true);
    archive.set_preserve_permissions(true);
    // Don't unpack outside mount_dir
    archive.set_unpack_xattrs(true);

    let mount_path = Path::new(mount_dir);

    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let path = entry.path().context("reading entry path")?.into_owned();

        // Handle OCI whiteout files
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            if file_name == ".wh..wh..opq" {
                // Opaque whiteout: delete all contents of the parent directory
                let parent = mount_path.join(path.parent().unwrap_or_else(|| Path::new(".")));
                if parent.exists() {
                    for child in std::fs::read_dir(&parent)? {
                        let child = child?;
                        let child_path = child.path();
                        if child_path.is_dir() {
                            let _ = std::fs::remove_dir_all(&child_path);
                        } else {
                            let _ = std::fs::remove_file(&child_path);
                        }
                    }
                }
                continue;
            }

            if let Some(target_name) = file_name.strip_prefix(".wh.") {
                // Regular whiteout: delete the specific file/dir
                let target = mount_path.join(
                    path.parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(target_name),
                );
                if target.is_dir() {
                    let _ = std::fs::remove_dir_all(&target);
                } else {
                    let _ = std::fs::remove_file(&target);
                }
                continue;
            }
        }

        // Normal entry: extract to mount directory
        entry
            .unpack_in(mount_path)
            .with_context(|| format!("unpacking {}", path.display()))?;
    }

    Ok(())
}

/// Install fc-runner agent binary into the rootfs if not already present.
async fn install_agent_if_missing(rootfs_path: &str) -> anyhow::Result<()> {
    let mount_dir = format!("{}-agent-install", rootfs_path);
    tokio::fs::create_dir_all(&mount_dir).await?;
    mount_ext4(rootfs_path, &mount_dir).await?;

    let agent_path = format!("{}/usr/local/bin/fc-runner", mount_dir);
    let needs_install = !Path::new(&agent_path).exists();

    if needs_install {
        tracing::info!("installing fc-runner agent into OCI-based rootfs");
        let self_exe = std::env::current_exe().context("getting current executable path")?;
        tokio::fs::create_dir_all(format!("{}/usr/local/bin", mount_dir)).await?;
        tokio::fs::copy(&self_exe, &agent_path).await?;
        std::fs::set_permissions(
            &agent_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
    }

    // Write entrypoint that uses fc-runner agent
    let entrypoint_path = format!("{}/entrypoint.sh", mount_dir);
    if needs_install || !Path::new(&entrypoint_path).exists() {
        tokio::fs::write(
            &entrypoint_path,
            "#!/bin/bash\nset -euo pipefail\nexec > /var/log/runner.log 2>&1\n\
             echo \"=== fc-runner entrypoint $(date) ===\"\n\
             if [ -x /usr/local/bin/fc-runner ]; then\n\
             \texec /usr/local/bin/fc-runner agent --log-level info\n\
             fi\n\
             echo \"fc-runner binary not found\"\nreboot -f\n",
        )
        .await?;
        std::fs::set_permissions(
            &entrypoint_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
    }

    // Ensure rc.local exists to boot the entrypoint
    let rc_local = format!("{}/etc/rc.local", mount_dir);
    if !Path::new(&rc_local).exists() {
        tokio::fs::create_dir_all(format!("{}/etc", mount_dir)).await?;
        tokio::fs::write(
            &rc_local,
            "#!/bin/bash\n/entrypoint.sh >> /var/log/runner.log 2>&1 &\nexit 0\n",
        )
        .await?;
        std::fs::set_permissions(
            &rc_local,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
    }

    // Ensure fstab uses /dev/vda (Firecracker block device)
    let fstab = format!("{}/etc/fstab", mount_dir);
    tokio::fs::write(&fstab, "/dev/vda\t/\text4\tdefaults,noatime\t0\t1\n").await?;

    // Mask serial-getty@ttyS0 — Firecracker has no real serial console device
    let mask_dir = format!("{}/etc/systemd/system", mount_dir);
    tokio::fs::create_dir_all(&mask_dir).await?;
    let mask_link = format!("{}/serial-getty@ttyS0.service", mask_dir);
    let _ = tokio::fs::remove_file(&mask_link).await;
    tokio::fs::symlink("/dev/null", &mask_link).await?;

    // Podman: create a wrapper at /usr/bin/docker that runs podman as root.
    // Running rootful avoids user namespace issues inside Firecracker VMs.
    let docker_wrapper = format!("{}/usr/bin/docker", mount_dir);
    let _ = tokio::fs::remove_file(&docker_wrapper).await;
    tokio::fs::write(
        &docker_wrapper,
        "#!/bin/sh\nexec sudo /usr/bin/podman \"$@\"\n",
    )
    .await?;
    std::fs::set_permissions(
        &docker_wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )?;

    // Podman containers.conf: configure DNS for container networking
    let containers_dir = format!("{}/etc/containers", mount_dir);
    tokio::fs::create_dir_all(&containers_dir).await?;
    tokio::fs::write(
        format!("{}/containers.conf", containers_dir),
        "[containers]\n\
         dns_servers = [\"8.8.8.8\", \"1.1.1.1\"]\n\
         \n\
         [engine]\n\
         cgroup_manager = \"cgroupfs\"\n\
         runtime = \"crun\"\n",
    )
    .await?;
    // Allow short image names like "postgres:16" to resolve to Docker Hub
    tokio::fs::write(
        format!("{}/registries.conf", containers_dir),
        "unqualified-search-registries = [\"docker.io\"]\nshort-name-mode = \"permissive\"\n",
    )
    .await?;
    // Image signature policy — accept all (required for podman pull)
    tokio::fs::write(
        format!("{}/policy.json", containers_dir),
        "{ \"default\": [{ \"type\": \"insecureAcceptAnything\" }] }\n",
    )
    .await?;

    // Install overlay-init script and directories for OverlayFS COW mode
    install_overlay_init_into(&mount_dir).await?;

    umount(&mount_dir).await?;
    let _ = tokio::fs::remove_dir(&mount_dir).await;

    if needs_install {
        tracing::info!("fc-runner agent installed into rootfs");
    } else {
        tracing::debug!("fc-runner agent already present in rootfs");
    }

    Ok(())
}

/// Install the overlay-init script and required directories into a mounted rootfs.
async fn install_overlay_init_into(mount_dir: &str) -> anyhow::Result<()> {
    // Create overlay directories
    for dir in ["/overlay/root", "/overlay/work", "/mnt", "/rom"] {
        tokio::fs::create_dir_all(format!("{}{}", mount_dir, dir)).await?;
    }

    let sbin_dir = format!("{}/sbin", mount_dir);
    tokio::fs::create_dir_all(&sbin_dir).await?;
    let overlay_init = format!("{}/overlay-init", sbin_dir);
    tokio::fs::write(
        &overlay_init,
        r#"#!/bin/sh
# overlay-init: OverlayFS COW boot for Firecracker VMs

echo "overlay-init: starting"

# Mount /proc so we can read kernel command line (not available as PID 1)
/bin/mount -t proc proc /proc

for arg in $(cat /proc/cmdline); do
    key="${arg%%=*}"
    val="${arg#*=}"
    case "$key" in
        overlay_root) overlay_root="$val" ;;
    esac
done

echo "overlay-init: overlay_root=$overlay_root"

pivot() {
    /bin/mount \
        -o noatime,lowerdir=/,upperdir="$1",workdir="$2" \
        -t overlay "overlayfs:$1" /mnt
    pivot_root /mnt /mnt/rom
}

if [ -z "$overlay_root" ]; then
    echo "overlay-init: no overlay_root, booting normally"
    exec /sbin/init "$@"
fi

if [ "$overlay_root" = "ram" ]; then
    /bin/mount -t tmpfs -o noatime,mode=0755 tmpfs /overlay
else
    if [ ! -b "/dev/$overlay_root" ]; then
        echo "FATAL: /dev/$overlay_root does not exist"
        exec /sbin/init "$@"
    fi
    /bin/mount -t ext4 -o noatime "/dev/$overlay_root" /overlay
fi

mkdir -p /overlay/root /overlay/work

# Unmount /proc before creating overlayfs
/bin/umount /proc

pivot /overlay/root /overlay/work

# Mount devtmpfs so we have access to /dev/vdb in the new root
/bin/mount -t devtmpfs devtmpfs /dev

# Mount overlay ext4 at a persistent path for container storage
# Podman/containers need writable storage not on overlayfs
mkdir -p /overlay-data
/bin/mount -t ext4 -o noatime /dev/vdb /overlay-data
mkdir -p /overlay-data/containers
# Point Podman graphroot to real ext4 so fuse-overlayfs works
mkdir -p /etc/containers
cat > /etc/containers/storage.conf <<STOR
[storage]
driver = "overlay"
graphroot = "/overlay-data/containers"
runroot = "/run/containers/storage"
STOR
mkdir -p /run/containers/storage

# Write /etc/hosts to fix "unable to resolve host" warnings
cat > /etc/hosts <<HOSTS
127.0.0.1 localhost localhost.localdomain
::1 localhost ip6-localhost ip6-loopback
HOSTS

# Pre-configure network directly via ip commands — systemd-networkd has
# issues reading config files on overlayfs. Parse the injected networkd
# config and apply it immediately so networking works before systemd starts.
/bin/mount -t proc proc /proc
/bin/mount -t sysfs sys /sys

if [ -f /etc/systemd/network/20-eth.network ]; then
    addr=$(grep '^Address=' /etc/systemd/network/20-eth.network | head -1 | cut -d= -f2)
    gw=$(grep '^Gateway=' /etc/systemd/network/20-eth.network | head -1 | cut -d= -f2)
    if [ -n "$addr" ] && [ -n "$gw" ]; then
        ip link set eth0 up
        ip addr add "$addr" dev eth0 2>/dev/null
        ip route add default via "$gw" 2>/dev/null
        echo "overlay-init: network pre-configured $addr gw $gw"
    fi
fi

# Diagnostic: verify network state
echo "overlay-init: ip addr:"
ip addr show eth0 2>&1
echo "overlay-init: ip route:"
ip route show 2>&1

/bin/umount /sys
/bin/umount /proc

echo "overlay-init: done, exec /sbin/init"
exec /sbin/init "$@"
"#,
    )
    .await?;
    std::fs::set_permissions(
        &overlay_init,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )?;
    tracing::debug!("installed overlay-init into rootfs");
    Ok(())
}

/// Create a blank ext4 filesystem image.
async fn create_ext4_image(path: &str, size_bytes: u64) -> anyhow::Result<()> {
    // Create sparse file
    let f = std::fs::File::create(path).context("creating image file")?;
    f.set_len(size_bytes).context("setting image size")?;
    drop(f);

    // Format as ext4
    let status = Command::new("mkfs.ext4")
        .args(["-F", "-q", path])
        .status()
        .await
        .context("running mkfs.ext4")?;
    ensure!(status.success(), "mkfs.ext4 failed");
    Ok(())
}

/// Mount an ext4 image via loop device.
async fn mount_ext4(image: &str, target: &str) -> anyhow::Result<()> {
    let status = Command::new("mount")
        .args(["-o", "loop,noatime", image, target])
        .status()
        .await
        .context("running mount")?;
    ensure!(status.success(), "mount failed: {} -> {}", image, target);
    Ok(())
}

/// Unmount a filesystem (lazy unmount for safety).
async fn umount(target: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        nix::mount::umount2(target, nix::mount::MntFlags::MNT_DETACH)
            .map_err(|e| anyhow::anyhow!("umount {} failed: {}", target, e))?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let status = Command::new("umount")
            .args(["-l", target])
            .status()
            .await
            .context("running umount")?;
        ensure!(status.success(), "umount failed: {}", target);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_image_reference() {
        let r: Reference = "ghcr.io/silafood/fc-runner-image:latest".parse().unwrap();
        assert_eq!(r.registry(), "ghcr.io");
        assert_eq!(r.repository(), "silafood/fc-runner-image");
        assert_eq!(r.tag().unwrap(), "latest");
    }

    #[test]
    fn parse_docker_hub_reference() {
        let r: Reference = "ubuntu:24.04".parse().unwrap();
        assert_eq!(r.repository(), "library/ubuntu");
        assert_eq!(r.tag().unwrap(), "24.04");
    }

    #[test]
    fn whiteout_detection() {
        assert!(".wh.somefile".starts_with(".wh."));
        assert!(".wh..wh..opq".starts_with(".wh."));
        assert_eq!(".wh.somefile".strip_prefix(".wh."), Some("somefile"));
    }

    #[test]
    fn extract_layer_handles_empty() {
        // Create a valid empty tar.gz
        let builder = tar::Builder::new(Vec::new());
        let tar_data = builder.into_inner().unwrap();
        let mut gz_data = Vec::new();
        {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_data, flate2::Compression::fast());
            encoder.write_all(&tar_data).unwrap();
            encoder.finish().unwrap();
        }

        let tmp = tempfile::tempdir().unwrap();
        let result = extract_layer(&gz_data, tmp.path().to_str().unwrap());
        assert!(result.is_ok());
    }
}
