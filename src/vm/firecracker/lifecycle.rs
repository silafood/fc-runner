use std::path::PathBuf;

use anyhow::Context;
use firecracker_rs_sdk::firecracker::FirecrackerOption;
use firecracker_rs_sdk::instance::Instance;
use firecracker_rs_sdk::jailer::JailerOption;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use super::MicroVm;
use super::VmRunContext;
use super::mount::{lazy_umount_sync, mount_loop_ext4_ro};
use super::process::{filename_str, is_process_alive, kill_process, path_str, reap_zombies};
use crate::vm::netlink;
use crate::vm::vsock;

impl MicroVm {
    /// Execute the VM using the provided run context.
    ///
    /// The context carries optional VSOCK notification and SSE log broadcast
    /// channels. When `vsock_notify` is set, the channel receives a notification
    /// as soon as the guest agent reports `JobCompleted`, allowing the
    /// orchestrator to begin replacement before the VM fully shuts down.
    pub async fn execute(self, env_content: &str, ctx: VmRunContext) -> anyhow::Result<()> {
        let result = self.prepare_and_run(env_content, ctx).await;
        self.cleanup().await;
        result
    }

    async fn prepare_and_run(&self, env_content: &str, ctx: VmRunContext) -> anyhow::Result<()> {
        let mmds = self.use_mmds();
        let use_jailer = self.fc_config.jailer_path.is_some();
        tracing::info!(
            vm_id = %self.vm_id,
            job_id = self.job_id,
            slot = self.slot,
            secret_injection = if mmds { "mmds" } else { "mount" },
            jailer = use_jailer,
            "preparing VM"
        );

        if self.use_overlay() {
            // Overlay mode: create sparse overlay ext4 (no rootfs copy needed)
            self.create_overlay().await?;
        } else {
            // Legacy mode: full rootfs copy
            self.copy_rootfs().await?;
        }

        // Ensure persistent cache image exists (creates once per slot, reused across VMs)
        self.ensure_cache_image().await?;

        if mmds {
            // MMDS mode: only inject network config to disk, secrets via MMDS
            self.inject_network_config().await?;
        } else {
            // Legacy mount mode: inject everything via loop mount
            self.inject_env_mount(env_content).await?;
        }

        self.create_tap().await?;

        // Spawn VSOCK listener before VM starts (if enabled)
        let vsock_handle = if self.fc_config.vsock_enabled {
            tracing::info!(vm_id = %self.vm_id, uds = %self.vsock_socket_path.display(), "spawning VSOCK listener");
            Some(vsock::spawn_listener(
                self.vm_id.clone(),
                self.vsock_socket_path.clone(),
                ctx.vsock_notify,
                ctx.log_tx,
            ))
        } else {
            None
        };

        tracing::info!(vm_id = %self.vm_id, "launching firecracker");

        // Helper: abort VSOCK listener and return an error.
        // Ensures the listener is always cleaned up, even on early return.
        let abort_vsock = |handle: &Option<tokio::task::JoinHandle<()>>| {
            if let Some(h) = handle {
                h.abort();
            }
        };

        let run_result = if mmds {
            // MMDS mode: use SDK for process management and API calls
            self.run_with_sdk(env_content).await
        } else {
            // No-API mode: use config file + tokio process (SDK requires API socket)
            if use_jailer {
                if let Err(e) = self.setup_jailer_chroot().await {
                    abort_vsock(&vsock_handle);
                    return Err(e);
                }
            } else {
                if let Err(e) = self.write_vm_config().await {
                    abort_vsock(&vsock_handle);
                    return Err(e);
                }
                if let Err(e) = tokio::fs::write(&self.log_path, "").await {
                    abort_vsock(&vsock_handle);
                    return Err(anyhow::Error::new(e).context("creating log file"));
                }
            }
            match self.run_no_api().await {
                Ok(exit_status) => {
                    use std::os::unix::process::ExitStatusExt;
                    if exit_status.success() {
                        Ok(())
                    } else if exit_status.signal().is_some() {
                        // Signal-based exit is normal when the guest does reboot -f:
                        // KVM_EXIT_SHUTDOWN causes Firecracker/jailer to exit via signal
                        tracing::info!(
                            vm_id = %self.vm_id,
                            signal = exit_status.signal(),
                            "VM exited via signal (normal for guest reboot)"
                        );
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!(
                            "firecracker exited with status {:?}",
                            exit_status.code()
                        ))
                    }
                }
                Err(e) => Err(e),
            }
        };

        // Abort VSOCK listener when VM exits (or on any path reaching here)
        abort_vsock(&vsock_handle);

        // Always dump guest log, regardless of how the VM exited
        self.dump_guest_log().await;

        match run_result {
            Ok(()) => {
                tracing::info!(vm_id = %self.vm_id, "VM completed successfully");
                Ok(())
            }
            Err(e) => {
                tracing::error!(vm_id = %self.vm_id, error = %e, "VM run failed");
                Err(e)
            }
        }
    }

    /// Create a per-VM TAP device with a unique subnet.
    async fn create_tap(&self) -> anyhow::Result<()> {
        tracing::info!(
            vm_id = %self.vm_id,
            tap = %self.tap_name,
            host_ip = %self.host_ip,
            guest_ip = %self.guest_ip,
            "creating per-VM TAP device"
        );

        // Delete if exists from a previous crashed VM, then create.
        // Retry once after a brief delay in case the delete is still in progress.
        let _ = netlink::delete_link(&self.tap_name).await;

        let tap_result = netlink::create_tap(&self.tap_name).await;
        if tap_result.is_err() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = netlink::delete_link(&self.tap_name).await;
            netlink::create_tap(&self.tap_name)
                .await
                .context("creating TAP device (retry)")?;
        }

        let ip: std::net::Ipv4Addr = self.host_ip.parse().context("parsing host IP")?;
        netlink::add_address_v4(&self.tap_name, ip, 24)
            .await
            .context("assigning IP to TAP")?;

        netlink::set_link_up(&self.tap_name)
            .await
            .context("bringing TAP up")?;

        Ok(())
    }

    /// Destroy the per-VM TAP device.
    async fn destroy_tap(&self) {
        tracing::info!(vm_id = %self.vm_id, tap = %self.tap_name, "destroying TAP device");
        let _ = netlink::delete_link(&self.tap_name).await;
    }

    /// Mount the VM rootfs after exit and dump /var/log/runner.log for debugging.
    async fn dump_guest_log(&self) {
        // Determine which image to mount for log retrieval
        let rootfs_location = if self.use_overlay() {
            // In overlay mode, guest writes go to the overlay ext4
            self.overlay_path.clone()
        } else if self.fc_config.jailer_path.is_some() {
            let name = match filename_str(&self.rootfs_path) {
                Ok(n) => n,
                Err(_) => return,
            };
            self.jailer_root_dir().join(name)
        } else {
            self.rootfs_path.clone()
        };
        let rootfs = match path_str(&rootfs_location) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mnt = match path_str(&self.mount_point) {
            Ok(s) => s.to_string(),
            Err(_) => return,
        };

        if tokio::fs::create_dir_all(&self.mount_point).await.is_err() {
            return;
        }

        // noload skips journal replay so we can mount a dirty ext4 after VM kill
        if let Err(e) = mount_loop_ext4_ro(rootfs, &mnt) {
            tracing::warn!(vm_id = %self.vm_id, error = %e, "mount failed for guest log dump");
            let _ = tokio::fs::remove_dir(&self.mount_point).await;
            return;
        }

        // In overlay mode, guest files are under root/ on the overlay ext4
        let log_path = if self.use_overlay() {
            self.mount_point.join("root/var/log/runner.log")
        } else {
            self.mount_point.join("var/log/runner.log")
        };
        match tokio::fs::read_to_string(&log_path).await {
            Ok(contents) => {
                // Strip ANSI escape sequences and log each line
                let lines: Vec<&str> = contents.lines().collect();
                for line in lines.iter().take(50) {
                    let clean: String = line
                        .chars()
                        .scan(false, |in_escape, c| {
                            if *in_escape {
                                if c.is_ascii_alphabetic() {
                                    *in_escape = false;
                                }
                                Some(None)
                            } else if c == '\x1b' {
                                *in_escape = true;
                                Some(None)
                            } else {
                                Some(Some(c))
                            }
                        })
                        .flatten()
                        .collect();
                    let clean = clean.trim();
                    if !clean.is_empty() {
                        tracing::info!(vm_id = %self.vm_id, "[guest-log] {}", clean);
                    }
                }
                if lines.len() > 50 {
                    tracing::info!(vm_id = %self.vm_id, "[guest-log] ... (truncated)");
                }
            }
            Err(e) => {
                tracing::warn!(vm_id = %self.vm_id, error = %e, "could not read guest runner.log");
            }
        }

        let _ = lazy_umount_sync(&mnt);
        let _ = tokio::fs::remove_dir(&self.mount_point).await;
    }

    /// Start Firecracker via SDK with API socket (MMDS mode).
    async fn run_with_sdk(&self, env_content: &str) -> anyhow::Result<()> {
        let mut instance = self.build_sdk_instance()?;

        // Pre-create log file so Firecracker can open it
        tokio::fs::write(&self.log_path, "")
            .await
            .context("creating log file")?;

        // start_vmm spawns the process and connects to the API socket
        instance
            .start_vmm()
            .await
            .map_err(|e| anyhow::anyhow!("start_vmm failed: {}", e))?;

        // Configure the VM via typed API calls
        if let Err(e) = self.configure_instance(&mut instance, env_content).await {
            tracing::error!(vm_id = %self.vm_id, error = %e, "VM configuration failed");
            let _ = instance.stop().await;
            return Err(e);
        }

        // Boot the VM
        instance
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("instance start failed: {}", e))?;

        tracing::info!(vm_id = %self.vm_id, "VM booted via SDK");

        // Wait for the VM process to exit
        self.wait_for_exit(&instance).await?;

        // Instance Drop will handle process cleanup (SIGTERM + file removal)
        Ok(())
    }

    /// Start Firecracker in --no-api mode (legacy mount injection).
    async fn run_no_api(&self) -> anyhow::Result<std::process::ExitStatus> {
        let mut child = if let Some(jailer_path) = &self.fc_config.jailer_path {
            let uid = self.fc_config.jailer_uid.expect("validated in config");
            let gid = self.fc_config.jailer_gid.expect("validated in config");
            let config_name =
                filename_str(&self.config_path).expect("config_path must have a valid filename");
            tracing::info!(
                vm_id = %self.vm_id,
                jailer = %jailer_path,
                uid, gid,
                config = %config_name,
                chroot_base = %self.fc_config.jailer_chroot_base,
                "launching via jailer (no-api mode)"
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
                .arg(config_name)
                .arg("--no-api")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("spawning jailer")?
        } else {
            Command::new(&self.fc_config.binary_path)
                .arg("--config-file")
                .arg(&self.config_path)
                .arg("--no-api")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("spawning firecracker")?
        };

        let wait_fut = child.wait();

        tokio::select! {
            result = timeout(Duration::from_secs(self.vm_timeout_secs), wait_fut) => {
                let status = result
                    .context("VM execution timed out")?
                    .context("waiting for firecracker")?;
                Ok(status)
            }
            _ = self.cancel.cancelled() => {
                tracing::info!(vm_id = %self.vm_id, "shutdown signal, killing firecracker child");
                let _ = child.kill().await;
                let status = child.wait().await.context("waiting after kill")?;
                Ok(status)
            }
        }
    }

    /// Wait for the firecracker process to exit by polling the PID.
    async fn wait_for_exit(&self, instance: &Instance) -> anyhow::Result<()> {
        let pid = instance
            .firecracker_pid()
            .ok_or_else(|| anyhow::anyhow!("no firecracker PID available"))?;

        tracing::info!(vm_id = %self.vm_id, pid, "waiting for firecracker process to exit");

        let wait_fut = async {
            loop {
                if !is_process_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };

        tokio::select! {
            result = timeout(Duration::from_secs(self.vm_timeout_secs), wait_fut) => {
                result.context("VM execution timed out")?;
                tracing::info!(vm_id = %self.vm_id, pid, "firecracker process exited");
            }
            _ = self.cancel.cancelled() => {
                tracing::info!(vm_id = %self.vm_id, pid, "shutdown signal, killing firecracker process");
                kill_process(pid);
                // Give it a moment to die
                tokio::time::sleep(Duration::from_millis(200)).await;
                if is_process_alive(pid) {
                    tracing::warn!(vm_id = %self.vm_id, pid, "SIGKILL after SIGTERM failed to kill process");
                }
            }
        }

        // Reap any zombie children left by the SDK/jailer.
        // The SDK spawns processes that become our children, but doesn't
        // always call waitpid() on exit. Reap them to prevent zombie buildup.
        reap_zombies();

        Ok(())
    }

    /// Create an SDK Instance using FirecrackerOption or JailerOption.
    fn build_sdk_instance(&self) -> anyhow::Result<Instance> {
        let use_jailer = self.fc_config.jailer_path.is_some();
        let sock_name = "api.sock";

        if use_jailer {
            let jailer_path = self.fc_config.jailer_path.as_ref().unwrap();
            let uid = self.fc_config.jailer_uid.expect("validated in config") as usize;
            let gid = self.fc_config.jailer_gid.expect("validated in config") as usize;

            let mut fc_opt = FirecrackerOption::new(&self.fc_config.binary_path);
            fc_opt.api_sock(sock_name);

            let mut jailer_opt = JailerOption::new(
                jailer_path,
                &self.fc_config.binary_path,
                &self.vm_id,
                gid,
                uid,
            );
            jailer_opt
                .chroot_base_dir(Some(&self.fc_config.jailer_chroot_base))
                .firecracker_option(Some(&fc_opt))
                .stdout("/dev/null")
                .stderr("/dev/null")
                .remove_jailer_workspace_dir();

            let instance = jailer_opt
                .build()
                .map_err(|e| anyhow::anyhow!("building jailer instance: {}", e))?;

            tracing::info!(
                vm_id = %self.vm_id,
                jailer = %jailer_path,
                uid, gid,
                chroot_base = %self.fc_config.jailer_chroot_base,
                "SDK instance created with jailer"
            );

            Ok(instance)
        } else {
            let socket_path = &self.socket_path;
            let mut fc_opt = FirecrackerOption::new(&self.fc_config.binary_path);
            fc_opt
                .api_sock(socket_path)
                .id(&self.vm_id)
                .stdout("/dev/null")
                .stderr("/dev/null");

            let instance = fc_opt
                .build()
                .map_err(|e| anyhow::anyhow!("building firecracker instance: {}", e))?;

            tracing::info!(
                vm_id = %self.vm_id,
                socket = %socket_path.display(),
                "SDK instance created (bare firecracker)"
            );

            Ok(instance)
        }
    }

    async fn cleanup(&self) {
        tracing::info!(vm_id = %self.vm_id, "cleaning up VM artifacts");

        // Destroy per-VM TAP device
        self.destroy_tap().await;

        let console_path = self.rootfs_path.with_extension("console");
        for path in [
            &self.rootfs_path,
            &self.config_path,
            &self.socket_path,
            &self.log_path,
            &console_path,
            &self.overlay_path,
            &self.vsock_socket_path,
        ] {
            if let Err(e) = tokio::fs::remove_file(path).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(vm_id = %self.vm_id, path = %path.display(), error = %e, "CLEANUP_FAILED");
            }
        }
        if let Err(e) = tokio::fs::remove_dir(&self.mount_point).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(vm_id = %self.vm_id, path = %self.mount_point.display(), error = %e, "mount point cleanup failed");
        }

        // Clean up jailer chroot if jailer was used (retry once if busy)
        if self.fc_config.jailer_path.is_some() {
            let chroot_dir = PathBuf::from(&self.fc_config.jailer_chroot_base)
                .join("firecracker")
                .join(&self.vm_id);
            if let Err(e) = tokio::fs::remove_dir_all(&chroot_dir).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::debug!(vm_id = %self.vm_id, error = %e, "jailer chroot cleanup failed, retrying after delay");
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Err(e2) = tokio::fs::remove_dir_all(&chroot_dir).await
                    && e2.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(vm_id = %self.vm_id, path = %chroot_dir.display(), error = %e2, "jailer chroot cleanup failed after retry");
                }
            }
        }
    }
}
