use anyhow::Context;
use firecracker_rs_sdk::instance::Instance;
use firecracker_rs_sdk::models::*;
use std::path::PathBuf;

use super::MicroVm;
use super::mmds::{put_mmds_raw, put_vsock_raw};
use super::process::{filename_str, path_str};

impl MicroVm {
    /// Build a Firecracker VM config as a JSON value (for --config-file / no-api mode).
    /// Uses SDK model types for type safety.
    pub(crate) fn build_vm_config(&self, use_jailer: bool) -> anyhow::Result<serde_json::Value> {
        let (kernel_path, rootfs_path, log_path) = if use_jailer {
            (
                filename_str(&self.rootfs_path)?.replace(".ext4", "-kernel"),
                filename_str(&self.rootfs_path)?.to_string(),
                filename_str(&self.log_path)?.to_string(),
            )
        } else {
            (
                self.fc_config.kernel_path.clone(),
                path_str(&self.rootfs_path)?.to_string(),
                path_str(&self.log_path)?.to_string(),
            )
        };

        // Build boot args — append overlay init params when in overlay mode
        let mut boot_args = if self.use_overlay() {
            format!(
                "{} init=/sbin/overlay-init overlay_root=vdb",
                self.fc_config.boot_args
            )
        } else {
            self.fc_config.boot_args.clone()
        };

        // Append cache device param when cache is enabled
        if self.cache_path.is_some() {
            boot_args = format!("{} cache_dev={}", boot_args, self.cache_device_name());
        }

        // Build drives array — overlay mode uses two drives, cache adds a third
        let mut drives_vec: Vec<serde_json::Value> = if self.use_overlay() {
            let squashfs = if use_jailer {
                filename_str(&self.rootfs_path)?.replace(".ext4", "-rootfs.squashfs")
            } else {
                self.squashfs_path()
            };
            let overlay = if use_jailer {
                filename_str(&self.overlay_path)?.to_string()
            } else {
                path_str(&self.overlay_path)?.to_string()
            };
            vec![
                serde_json::json!({
                    "drive_id": "rootfs",
                    "path_on_host": squashfs,
                    "is_root_device": true,
                    "is_read_only": true
                }),
                serde_json::json!({
                    "drive_id": "overlay",
                    "path_on_host": overlay,
                    "is_root_device": false,
                    "is_read_only": false
                }),
            ]
        } else {
            vec![serde_json::json!({
                "drive_id": "rootfs",
                "path_on_host": rootfs_path,
                "is_root_device": true,
                "is_read_only": false
            })]
        };

        // Add cache drive when enabled
        if let Some(cache_path) = &self.cache_path {
            let cache_host_path = if use_jailer {
                format!("slot-{}-cache.ext4", self.slot)
            } else {
                path_str(cache_path)?.to_string()
            };
            drives_vec.push(serde_json::json!({
                "drive_id": "cache",
                "path_on_host": cache_host_path,
                "is_root_device": false,
                "is_read_only": false
            }));
        }

        let drives = serde_json::Value::Array(drives_vec);

        let mut config = serde_json::json!({
            "boot-source": {
                "kernel_image_path": kernel_path,
                "boot_args": boot_args
            },
            "drives": drives,
            "machine-config": {
                "vcpu_count": self.fc_config.vcpu_count,
                "mem_size_mib": self.fc_config.mem_size_mib
            },
            "network-interfaces": [{
                "iface_id": "eth0",
                "guest_mac": self.guest_mac,
                "host_dev_name": self.tap_name
            }],
            "logger": {
                "log_path": log_path,
                "level": self.fc_config.log_level
            }
        });

        // Enable MMDS when using MMDS injection mode
        if self.use_mmds() {
            config["mmds-config"] = serde_json::json!({
                "version": "V2",
                "network_interfaces": ["eth0"]
            });
        }

        // Add VSOCK device when enabled
        if self.fc_config.vsock_enabled {
            let cid = self.vsock_cid();
            let uds_path = if use_jailer {
                "vsock.sock".to_string()
            } else {
                path_str(&self.vsock_socket_path)?.to_string()
            };
            config["vsock"] = serde_json::json!({
                "guest_cid": cid,
                "uds_path": uds_path
            });
            tracing::info!(
                vm_id = %self.vm_id,
                cid,
                uds_path,
                "VSOCK device configured"
            );
        }

        Ok(config)
    }

    pub(crate) async fn write_vm_config(&self) -> anyhow::Result<()> {
        let use_jailer = self.fc_config.jailer_path.is_some();
        let config = self.build_vm_config(use_jailer)?;
        let rendered = serde_json::to_string_pretty(&config).context("serializing VM config")?;
        tokio::fs::write(&self.config_path, rendered).await?;
        Ok(())
    }

    /// Configure a running Firecracker instance via the SDK API.
    ///
    /// Calls the typed SDK methods to set machine config, boot source, drives,
    /// network interfaces, logger, MMDS, and VSOCK via the Firecracker API socket.
    pub(crate) async fn configure_instance(
        &self,
        instance: &mut Instance,
        env_content: &str,
    ) -> anyhow::Result<()> {
        // Machine configuration
        instance
            .put_machine_configuration(&MachineConfiguration {
                vcpu_count: self.fc_config.vcpu_count as isize,
                mem_size_mib: self.fc_config.mem_size_mib as isize,
                cpu_template: None,
                smt: None,
                track_dirty_pages: None,
                huge_pages: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("put_machine_configuration failed: {}", e))?;

        // Boot source — append overlay init params when in overlay mode
        let mut boot_args = if self.use_overlay() {
            format!(
                "{} init=/sbin/overlay-init overlay_root=vdb",
                self.fc_config.boot_args
            )
        } else {
            self.fc_config.boot_args.clone()
        };
        if self.cache_path.is_some() {
            boot_args = format!("{} cache_dev={}", boot_args, self.cache_device_name());
        }
        instance
            .put_guest_boot_source(&BootSource {
                kernel_image_path: PathBuf::from(&self.fc_config.kernel_path),
                boot_args: Some(boot_args),
                initrd_path: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("put_guest_boot_source failed: {}", e))?;

        if self.use_overlay() {
            // Overlay mode: squashfs (read-only root) + overlay ext4 (read-write)
            instance
                .put_guest_drive_by_id(&Drive {
                    drive_id: "rootfs".to_string(),
                    path_on_host: PathBuf::from(self.squashfs_path()),
                    is_root_device: true,
                    is_read_only: true,
                    partuuid: None,
                    cache_type: None,
                    rate_limiter: None,
                    io_engine: None,
                    socket: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_guest_drive_by_id (squashfs) failed: {}", e))?;

            instance
                .put_guest_drive_by_id(&Drive {
                    drive_id: "overlay".to_string(),
                    path_on_host: self.overlay_path.clone(),
                    is_root_device: false,
                    is_read_only: false,
                    partuuid: None,
                    cache_type: None,
                    rate_limiter: None,
                    io_engine: None,
                    socket: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_guest_drive_by_id (overlay) failed: {}", e))?;

            tracing::info!(vm_id = %self.vm_id, "overlay drives configured: squashfs (ro) + overlay ext4 (rw)");
        } else {
            // Legacy mode: single read-write rootfs
            instance
                .put_guest_drive_by_id(&Drive {
                    drive_id: "rootfs".to_string(),
                    path_on_host: self.rootfs_path.clone(),
                    is_root_device: true,
                    is_read_only: false,
                    partuuid: None,
                    cache_type: None,
                    rate_limiter: None,
                    io_engine: None,
                    socket: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_guest_drive_by_id failed: {}", e))?;
        }

        // Cache drive (persistent per-slot)
        if let Some(cache_path) = &self.cache_path {
            instance
                .put_guest_drive_by_id(&Drive {
                    drive_id: "cache".to_string(),
                    path_on_host: cache_path.clone(),
                    is_root_device: false,
                    is_read_only: false,
                    partuuid: None,
                    cache_type: None,
                    rate_limiter: None,
                    io_engine: None,
                    socket: None,
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_guest_drive_by_id (cache) failed: {}", e))?;
            tracing::info!(
                vm_id = %self.vm_id,
                cache_dev = self.cache_device_name(),
                path = %cache_path.display(),
                "cache drive configured"
            );
        }

        // Network interface
        instance
            .put_guest_network_interface_by_id(&NetworkInterface {
                iface_id: "eth0".to_string(),
                guest_mac: Some(self.guest_mac.clone()),
                host_dev_name: PathBuf::from(&self.tap_name),
                rx_rate_limiter: None,
                tx_rate_limiter: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("put_guest_network_interface_by_id failed: {}", e))?;

        // Logger
        instance
            .put_logger(&Logger {
                log_path: self.log_path.clone(),
                level: Some(log_level_from_str(&self.fc_config.log_level)),
                show_level: None,
                show_log_origin: None,
                module: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("put_logger failed: {}", e))?;

        // MMDS configuration and metadata
        if self.use_mmds() {
            instance
                .put_mmds_config(&MmdsConfig {
                    version: Some(MmdsConfigVersion::V2),
                    ipv4_address: None,
                    network_interfaces: vec!["eth0".to_string()],
                })
                .await
                .map_err(|e| anyhow::anyhow!("put_mmds_config failed: {}", e))?;

            // Send MMDS metadata via raw HTTP on the API socket.
            let mmds_json = self.build_mmds_payload(env_content)?;
            let socket_path = self.api_socket_path();
            put_mmds_raw(&socket_path, &mmds_json).await?;

            tracing::info!(vm_id = %self.vm_id, "MMDS metadata injected via SDK");
        }

        // VSOCK device — use raw HTTP API instead of SDK because the SDK
        // incorrectly deserializes Firecracker's `{}` response as unit struct.
        if self.fc_config.vsock_enabled {
            let cid = self.vsock_cid();
            let api_sock = self.api_socket_path();
            let vsock_uds = path_str(&self.vsock_socket_path)?;
            let vsock_json = serde_json::json!({
                "guest_cid": cid,
                "uds_path": vsock_uds
            })
            .to_string();
            put_vsock_raw(&api_sock, &vsock_json).await?;
            tracing::info!(vm_id = %self.vm_id, cid, vsock_uds, "VSOCK device configured");
        }

        Ok(())
    }
}

/// Parse a log level string into the SDK's LogLevel enum.
pub(crate) fn log_level_from_str(s: &str) -> LogLevel {
    match s.to_lowercase().as_str() {
        "error" => LogLevel::Error,
        "warning" | "warn" => LogLevel::Warning,
        "info" => LogLevel::Info,
        "debug" => LogLevel::Debug,
        other => {
            tracing::warn!(value = other, "unknown log_level, defaulting to Warning");
            LogLevel::Warning
        }
    }
}
