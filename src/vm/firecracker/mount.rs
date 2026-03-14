use anyhow::{Context, ensure};

/// Mount an ext4 image via loop (read-write).
///
/// Loop device setup is handled by the `mount` command (userspace), not the
/// kernel mount(2) syscall, so we must use Command here.
pub(crate) fn mount_loop_ext4(image: &str, target: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("mount")
        .args(["-o", "loop", image, target])
        .status()
        .context("running mount")?;
    ensure!(
        status.success(),
        "mount -o loop {} {} failed",
        image,
        target
    );
    Ok(())
}

/// Mount an ext4 image via loop (read-only, noload for dirty fs).
pub(crate) fn mount_loop_ext4_ro(image: &str, target: &str) -> anyhow::Result<()> {
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
pub(crate) fn try_umount(target: &str) -> bool {
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
pub(crate) fn lazy_umount_sync(target: &str) -> anyhow::Result<()> {
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
