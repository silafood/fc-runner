mod config_builder;
mod injection;
mod jailer;
mod lifecycle;
mod mmds;
mod mount;
mod process;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::api::VmLogEvent;
use crate::config::{AppConfig, FirecrackerConfig, NetworkConfig};
use crate::github::GitHubClient;
use crate::vm::vsock;

/// Shared context passed to VM runner functions, reducing parameter count.
pub struct VmRunContext {
    pub config: Arc<AppConfig>,
    pub github: Arc<GitHubClient>,
    pub slot: usize,
    pub cancel: CancellationToken,
    pub log_tx: Option<broadcast::Sender<VmLogEvent>>,
    /// VSOCK notification channel for early job-completion signaling.
    pub vsock_notify: Option<tokio::sync::mpsc::Sender<vsock::JobDoneNotification>>,
    /// Per-pool vCPU override (only used by named pools).
    pub vcpu_override: Option<u32>,
    /// Per-pool memory override (only used by named pools).
    pub mem_override: Option<u32>,
}

impl VmRunContext {
    pub fn new(
        config: Arc<AppConfig>,
        github: Arc<GitHubClient>,
        slot: usize,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            github,
            slot,
            cancel,
            log_tx: None,
            vsock_notify: None,
            vcpu_override: None,
            mem_override: None,
        }
    }

    pub fn log_tx(mut self, tx: broadcast::Sender<VmLogEvent>) -> Self {
        self.log_tx = Some(tx);
        self
    }

    pub fn vsock_notify(
        mut self,
        tx: tokio::sync::mpsc::Sender<vsock::JobDoneNotification>,
    ) -> Self {
        self.vsock_notify = Some(tx);
        self
    }

    pub fn vcpu_override(mut self, vcpu: u32) -> Self {
        self.vcpu_override = Some(vcpu);
        self
    }

    pub fn mem_override(mut self, mem: u32) -> Self {
        self.mem_override = Some(mem);
        self
    }
}

/// S3 configuration to inject into guest VMs for runs-on/cache direct uploads.
#[derive(Clone, Debug)]
pub struct S3GuestConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
}

pub struct MicroVm {
    pub vm_id: String,
    pub job_id: u64,
    pub slot: usize,
    rootfs_path: PathBuf,
    config_path: PathBuf,
    socket_path: PathBuf,
    log_path: PathBuf,
    mount_point: PathBuf,
    /// Per-VM overlay ext4 file (used only when overlay_rootfs = true).
    overlay_path: PathBuf,
    /// Per-VM VSOCK Unix socket path (used only when vsock_enabled = true).
    vsock_socket_path: PathBuf,
    /// Per-slot persistent cache ext4 image (used only when cache_enabled = true).
    cache_path: Option<PathBuf>,
    /// Token for the Actions cache service (set when cache_service is enabled).
    pub cache_service_token: Option<String>,
    /// Port the cache service listens on (from server.listen_addr).
    pub cache_service_port: Option<u16>,
    /// S3 config for runs-on/cache direct uploads (injected via MMDS/mount).
    pub s3_config: Option<S3GuestConfig>,
    fc_config: FirecrackerConfig,
    vm_timeout_secs: u64,
    cancel: CancellationToken,
    // Per-VM networking
    tap_name: String,
    pub host_ip: String,
    guest_ip: String,
    guest_mac: String,
    network_dns: Vec<String>,
}

impl MicroVm {
    pub fn new(
        job_id: u64,
        fc_config: &FirecrackerConfig,
        network: &NetworkConfig,
        work_dir: &str,
        vm_timeout_secs: u64,
        slot: usize,
        cancel: CancellationToken,
    ) -> Self {
        let vm_id = format!("fc-{}-{}", job_id, Uuid::new_v4().simple());
        let base = PathBuf::from(work_dir);
        let cache_path = if fc_config.cache_enabled {
            Some(PathBuf::from(&fc_config.cache_dir).join(format!("slot-{}.ext4", slot)))
        } else {
            None
        };
        Self {
            rootfs_path: base.join(format!("{}.ext4", vm_id)),
            config_path: base.join(format!("{}.json", vm_id)),
            socket_path: base.join(format!("{}.sock", vm_id)),
            log_path: base.join(format!("{}.log", vm_id)),
            mount_point: base.join(format!("{}.mnt", vm_id)),
            overlay_path: base.join(format!("{}.overlay.ext4", vm_id)),
            vsock_socket_path: base.join(format!("{}.vsock", vm_id)),
            cache_path,
            cache_service_token: None,
            cache_service_port: None,
            s3_config: None,
            job_id,
            vm_id,
            fc_config: fc_config.clone(),
            vm_timeout_secs,
            cancel,
            slot,
            tap_name: format!("tap-fc{}", slot),
            host_ip: format!("172.16.{}.1", slot),
            guest_ip: format!("172.16.{}.2", slot),
            guest_mac: format!("06:00:AC:10:{:02X}:02", slot),
            network_dns: network.dns.clone(),
        }
    }

    fn use_mmds(&self) -> bool {
        self.fc_config.secret_injection == "mmds"
    }

    fn use_overlay(&self) -> bool {
        self.fc_config.overlay_rootfs
    }

    /// Path to the shared squashfs rootfs (derived from golden rootfs path).
    fn squashfs_path(&self) -> String {
        self.fc_config.rootfs_golden.replace(".ext4", ".squashfs")
    }

    /// Guest device name for the cache volume.
    /// In overlay mode: vda=squashfs, vdb=overlay, vdc=cache
    /// In legacy mode: vda=rootfs, vdb=cache
    fn cache_device_name(&self) -> &str {
        if self.use_overlay() { "vdc" } else { "vdb" }
    }

    /// Compute the VSOCK CID for this VM (cid_base + slot).
    fn vsock_cid(&self) -> u32 {
        self.fc_config.vsock_cid_base + self.slot as u32
    }

    /// Returns the jailer chroot root directory: <chroot_base>/firecracker/<vm_id>/root
    fn jailer_root_dir(&self) -> PathBuf {
        PathBuf::from(&self.fc_config.jailer_chroot_base)
            .join("firecracker")
            .join(&self.vm_id)
            .join("root")
    }

    /// Resolve the Firecracker API socket path (differs between jailer and bare mode).
    fn api_socket_path(&self) -> PathBuf {
        if self.fc_config.jailer_path.is_some() {
            self.jailer_root_dir().join("api.sock")
        } else {
            self.socket_path.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FirecrackerConfig, NetworkConfig};

    fn test_fc_config(overlay: bool) -> FirecrackerConfig {
        FirecrackerConfig {
            binary_path: "/usr/local/bin/firecracker".to_string(),
            kernel_path: "/opt/fc-runner/vmlinux.bin".to_string(),
            rootfs_golden: "/opt/fc-runner/runner-rootfs-golden.ext4".to_string(),
            image: None,
            vcpu_count: 2,
            mem_size_mib: 2048,
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off".to_string(),
            log_level: "Warning".to_string(),
            jailer_path: None,
            jailer_uid: None,
            jailer_gid: None,
            jailer_chroot_base: "/var/lib/fc-runner/jailer".to_string(),
            kernel_url: None,
            cloud_img_url: None,
            secret_injection: "mmds".to_string(),
            vsock_enabled: false,
            vsock_cid_base: 3,
            overlay_rootfs: overlay,
            overlay_size_mib: 512,
            cache_enabled: false,
            cache_size_mib: 2048,
            cache_dir: "/var/lib/fc-runner/cache".to_string(),
        }
    }

    fn test_network_config() -> NetworkConfig {
        NetworkConfig {
            host_ip: "172.16.0.1".to_string(),
            guest_ip: "172.16.0.2".to_string(),
            cidr: "24".to_string(),
            dns: vec!["8.8.8.8".to_string()],
            allowed_networks: vec![],
            resolved_networks: vec![],
        }
    }

    fn create_test_vm(overlay: bool) -> MicroVm {
        let fc_config = test_fc_config(overlay);
        let network = test_network_config();
        MicroVm::new(
            12345,
            &fc_config,
            &network,
            "/tmp/fc-test",
            3600,
            0,
            CancellationToken::new(),
        )
    }

    #[test]
    fn overlay_path_set_on_new() {
        let vm = create_test_vm(true);
        assert!(vm.overlay_path.to_string_lossy().contains(".overlay.ext4"));
    }

    #[test]
    fn use_overlay_returns_correct_value() {
        let vm_overlay = create_test_vm(true);
        assert!(vm_overlay.use_overlay());

        let vm_no_overlay = create_test_vm(false);
        assert!(!vm_no_overlay.use_overlay());
    }

    #[test]
    fn squashfs_path_derived_from_golden() {
        let vm = create_test_vm(true);
        assert_eq!(
            vm.squashfs_path(),
            "/opt/fc-runner/runner-rootfs-golden.squashfs"
        );
    }

    #[test]
    fn build_vm_config_legacy_single_drive() {
        let vm = create_test_vm(false);
        let config = vm.build_vm_config(false).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[0]["is_read_only"], false);
        assert!(
            !config["boot-source"]["boot_args"]
                .as_str()
                .unwrap()
                .contains("overlay-init")
        );
    }

    #[test]
    fn build_vm_config_overlay_two_drives() {
        let vm = create_test_vm(true);
        let config = vm.build_vm_config(false).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 2);

        // First drive: squashfs, read-only root
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[0]["is_root_device"], true);
        assert_eq!(drives[0]["is_read_only"], true);
        assert!(
            drives[0]["path_on_host"]
                .as_str()
                .unwrap()
                .ends_with(".squashfs")
        );

        // Second drive: overlay, read-write
        assert_eq!(drives[1]["drive_id"], "overlay");
        assert_eq!(drives[1]["is_root_device"], false);
        assert_eq!(drives[1]["is_read_only"], false);
        assert!(
            drives[1]["path_on_host"]
                .as_str()
                .unwrap()
                .contains(".overlay.ext4")
        );
    }

    #[test]
    fn build_vm_config_overlay_boot_args() {
        let vm = create_test_vm(true);
        let config = vm.build_vm_config(false).unwrap();
        let boot_args = config["boot-source"]["boot_args"].as_str().unwrap();
        assert!(boot_args.contains("init=/sbin/overlay-init"));
        assert!(boot_args.contains("overlay_root=vdb"));
        // Original args still present
        assert!(boot_args.contains("console=ttyS0"));
    }

    #[test]
    fn build_vm_config_legacy_no_overlay_boot_args() {
        let vm = create_test_vm(false);
        let config = vm.build_vm_config(false).unwrap();
        let boot_args = config["boot-source"]["boot_args"].as_str().unwrap();
        assert!(!boot_args.contains("overlay-init"));
        assert!(!boot_args.contains("overlay_root"));
    }

    fn create_test_vm_with_cache(overlay: bool) -> MicroVm {
        let mut config = test_fc_config(overlay);
        config.cache_enabled = true;
        config.cache_dir = "/var/lib/fc-runner/cache".to_string();
        MicroVm::new(
            12345,
            &config,
            &test_network_config(),
            "/tmp/fc-test",
            3600,
            0,
            CancellationToken::new(),
        )
    }

    #[test]
    fn cache_path_set_when_enabled() {
        let vm = create_test_vm_with_cache(true);
        assert!(vm.cache_path.is_some());
        assert!(
            vm.cache_path
                .unwrap()
                .to_string_lossy()
                .contains("slot-0.ext4")
        );
    }

    #[test]
    fn cache_path_none_when_disabled() {
        let vm = create_test_vm(true);
        assert!(vm.cache_path.is_none());
    }

    #[test]
    fn cache_device_name_overlay_mode() {
        let vm = create_test_vm_with_cache(true);
        assert_eq!(vm.cache_device_name(), "vdc");
    }

    #[test]
    fn cache_device_name_legacy_mode() {
        let vm = create_test_vm_with_cache(false);
        assert_eq!(vm.cache_device_name(), "vdb");
    }

    #[test]
    fn build_vm_config_overlay_with_cache_three_drives() {
        let vm = create_test_vm_with_cache(true);
        let config = vm.build_vm_config(false).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 3);
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[1]["drive_id"], "overlay");
        assert_eq!(drives[2]["drive_id"], "cache");
        assert_eq!(drives[2]["is_read_only"], false);
        assert!(
            drives[2]["path_on_host"]
                .as_str()
                .unwrap()
                .contains("slot-0.ext4")
        );
    }

    #[test]
    fn build_vm_config_legacy_with_cache_two_drives() {
        let vm = create_test_vm_with_cache(false);
        let config = vm.build_vm_config(false).unwrap();
        let drives = config["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 2);
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[1]["drive_id"], "cache");
    }

    #[test]
    fn build_vm_config_cache_boot_args() {
        let vm = create_test_vm_with_cache(true);
        let config = vm.build_vm_config(false).unwrap();
        let boot_args = config["boot-source"]["boot_args"].as_str().unwrap();
        assert!(boot_args.contains("cache_dev=vdc"));
    }

    #[test]
    fn build_vm_config_no_cache_boot_args_when_disabled() {
        let vm = create_test_vm(true);
        let config = vm.build_vm_config(false).unwrap();
        let boot_args = config["boot-source"]["boot_args"].as_str().unwrap();
        assert!(!boot_args.contains("cache_dev"));
    }

    #[test]
    fn log_level_parsing() {
        use config_builder::log_level_from_str;
        use firecracker_rs_sdk::models::LogLevel;
        assert!(matches!(log_level_from_str("error"), LogLevel::Error));
        assert!(matches!(log_level_from_str("Error"), LogLevel::Error));
        assert!(matches!(log_level_from_str("warning"), LogLevel::Warning));
        assert!(matches!(log_level_from_str("warn"), LogLevel::Warning));
        assert!(matches!(log_level_from_str("info"), LogLevel::Info));
        assert!(matches!(log_level_from_str("debug"), LogLevel::Debug));
        assert!(matches!(log_level_from_str("unknown"), LogLevel::Warning));
    }
}
