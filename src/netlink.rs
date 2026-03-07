//! Pure-Rust networking helpers for TAP device management.
//!
//! On Linux: uses rtnetlink + nix ioctl (no external commands).
//! On other platforms: falls back to `ip` commands for dev/testing.

use anyhow::ensure;
use std::net::Ipv4Addr;

// ── Linux: native rtnetlink + ioctl ──────────────────────────────────────────

#[cfg(target_os = "linux")]
mod inner {
    use super::*;
    use anyhow::Context;
    use futures::stream::TryStreamExt;

    /// Create a TAP device via ioctl(TUNSETIFF) on /dev/net/tun.
    pub async fn create_tap(name: &str) -> anyhow::Result<()> {
        use nix::fcntl::{OFlag, open};
        use nix::sys::stat::Mode;
        use std::os::fd::AsRawFd;

        const IFF_TAP: libc::c_short = 0x0002;
        const IFF_NO_PI: libc::c_short = 0x1000;
        const TUNSETIFF: libc::c_ulong = 0x400454ca;

        let fd = open(c"/dev/net/tun", OFlag::O_RDWR, Mode::empty())
            .context("opening /dev/net/tun")?;

        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        let name_bytes = name.as_bytes();
        ensure!(name_bytes.len() < libc::IFNAMSIZ, "TAP name too long: {}", name);
        unsafe {
            std::ptr::copy_nonoverlapping(
                name_bytes.as_ptr(),
                ifr.ifr_name.as_mut_ptr() as *mut u8,
                name_bytes.len(),
            );
            ifr.ifr_ifru.ifru_flags = IFF_TAP | IFF_NO_PI;
        }

        let ret = unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETIFF as _, &ifr as *const libc::ifreq) };
        if ret < 0 {
            anyhow::bail!("ioctl TUNSETIFF failed for {}: {}", name, std::io::Error::last_os_error());
        }

        // We must also set IFF_PERSIST so the TAP survives closing the fd.
        // Without this, closing fd destroys the device.
        const TUNSETPERSIST: libc::c_ulong = 0x400454cb;
        let ret = unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETPERSIST as _, 1 as libc::c_int) };
        if ret < 0 {
            anyhow::bail!("ioctl TUNSETPERSIST failed for {}: {}", name, std::io::Error::last_os_error());
        }

        // Close fd — the TAP persists due to TUNSETPERSIST.
        // Firecracker will re-open it by name. Cleaned up by delete_link() on VM teardown.
        drop(fd);
        Ok(())
    }

    /// Delete a network link by name. Silently succeeds if the link doesn't exist.
    pub async fn delete_link(name: &str) -> anyhow::Result<()> {
        let (conn, handle, _) = rtnetlink::new_connection()?;
        tokio::spawn(conn);

        let link = handle
            .link()
            .get()
            .match_name(name.to_string())
            .execute()
            .try_next()
            .await;

        if let Ok(Some(link)) = link {
            handle
                .link()
                .del(link.header.index)
                .execute()
                .await
                .with_context(|| format!("deleting link {}", name))?;
        }
        Ok(())
    }

    /// Assign an IPv4 address with prefix length to a named interface.
    pub async fn add_address_v4(name: &str, addr: Ipv4Addr, prefix_len: u8) -> anyhow::Result<()> {
        let (conn, handle, _) = rtnetlink::new_connection()?;
        tokio::spawn(conn);

        let link = handle
            .link()
            .get()
            .match_name(name.to_string())
            .execute()
            .try_next()
            .await?
            .with_context(|| format!("interface {} not found", name))?;

        handle
            .address()
            .add(link.header.index, addr.into(), prefix_len)
            .execute()
            .await
            .with_context(|| format!("adding address to {}", name))?;

        Ok(())
    }

    /// Bring a named interface up.
    pub async fn set_link_up(name: &str) -> anyhow::Result<()> {
        use rtnetlink::LinkUnspec;

        let (conn, handle, _) = rtnetlink::new_connection()?;
        tokio::spawn(conn);

        let link = handle
            .link()
            .get()
            .match_name(name.to_string())
            .execute()
            .try_next()
            .await?
            .with_context(|| format!("interface {} not found", name))?;

        let msg = LinkUnspec::new_with_index(link.header.index).up().build();
        handle
            .link()
            .change(msg)
            .execute()
            .await
            .with_context(|| format!("bringing {} up", name))?;

        Ok(())
    }

    /// Check if a named link exists.
    pub async fn link_exists(name: &str) -> bool {
        let Ok((conn, handle, _)) = rtnetlink::new_connection() else {
            return false;
        };
        tokio::spawn(conn);

        handle
            .link()
            .get()
            .match_name(name.to_string())
            .execute()
            .try_next()
            .await
            .ok()
            .flatten()
            .is_some()
    }
}

// ── Non-Linux fallback (macOS dev) ───────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
mod inner {
    use super::*;
    use tokio::process::Command;

    pub async fn create_tap(name: &str) -> anyhow::Result<()> {
        let status = Command::new("ip")
            .args(["tuntap", "add", "dev", name, "mode", "tap"])
            .status()
            .await?;
        ensure!(status.success(), "ip tuntap add failed for {}", name);
        Ok(())
    }

    pub async fn delete_link(name: &str) -> anyhow::Result<()> {
        let _ = Command::new("ip")
            .args(["link", "delete", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        Ok(())
    }

    pub async fn add_address_v4(name: &str, addr: Ipv4Addr, prefix_len: u8) -> anyhow::Result<()> {
        let status = Command::new("ip")
            .args(["addr", "add", &format!("{}/{}", addr, prefix_len), "dev", name])
            .status()
            .await?;
        ensure!(status.success(), "ip addr add failed for {}", name);
        Ok(())
    }

    pub async fn set_link_up(name: &str) -> anyhow::Result<()> {
        let status = Command::new("ip")
            .args(["link", "set", name, "up"])
            .status()
            .await?;
        ensure!(status.success(), "ip link set up failed for {}", name);
        Ok(())
    }

    pub async fn link_exists(name: &str) -> bool {
        Command::new("ip")
            .args(["link", "show", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

// ── Public re-exports ────────────────────────────────────────────────────────

pub use inner::*;
