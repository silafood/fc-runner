/// Check if a process is still alive by sending signal 0.
pub(crate) fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // Try waitpid(WNOHANG) first — this reaps zombies and detects exit.
        // kill(pid, 0) is insufficient because it returns Ok for zombies.
        match nix::sys::wait::waitpid(
            nix::unistd::Pid::from_raw(pid as i32),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        ) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) => true,
            Ok(_) => false, // exited, signaled, or stopped — not alive
            Err(nix::errno::Errno::ECHILD) => {
                // Not our child (or already reaped). Fall back to kill(0)
                // but also check /proc/{pid}/status for zombie state.
                if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err() {
                    return false;
                }
                // Process exists but isn't our child — check for zombie
                std::fs::read_to_string(format!("/proc/{}/status", pid))
                    .map(|s| !s.contains("\nState:\tZ"))
                    .unwrap_or(false)
            }
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux (e.g. macOS for dev), check if /proc/{pid} exists
        // or just assume alive (Firecracker only runs on Linux anyway)
        std::path::Path::new(&format!("/proc/{}", pid)).exists()
    }
}

/// Kill a process by PID (SIGKILL for immediate termination).
pub(crate) fn kill_process(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

/// Reap all zombie child processes to prevent buildup.
/// The SDK spawns firecracker/jailer as children but may not waitpid() on exit.
pub(crate) fn reap_zombies() {
    #[cfg(target_os = "linux")]
    {
        use nix::sys::wait::{WaitPidFlag, waitpid};
        use nix::unistd::Pid;

        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(nix::sys::wait::WaitStatus::StillAlive) => break,
                Ok(status) => {
                    tracing::debug!(?status, "reaped zombie child process");
                }
                Err(_) => break, // ECHILD = no more children
            }
        }
    }
}

/// Convert a PathBuf to &str with a descriptive error instead of panicking.
pub(crate) fn path_str(path: &std::path::Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path contains invalid UTF-8: {}", path.display()))
}

/// Extract the filename from a path as a &str, with a descriptive error.
pub(crate) fn filename_str(path: &std::path::Path) -> anyhow::Result<&str> {
    path.file_name()
        .ok_or_else(|| anyhow::anyhow!("path has no filename: {}", path.display()))?
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("filename contains invalid UTF-8: {}", path.display()))
}
