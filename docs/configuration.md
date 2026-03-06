# Configuration Reference

fc-runner is configured via a TOML file, default path `/etc/fc-runner/config.toml`.

You can pass a custom path as the first CLI argument:

```bash
fc-runner /path/to/config.toml
```

## Full Example

```toml
[github]
token = "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
owner = "your-org"
repo = "your-repo"
runner_group_id = 1
labels = ["self-hosted", "linux", "firecracker"]

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/runner-rootfs-golden.ext4"
vcpu_count = 2
mem_size_mib = 2048
tap_interface = "tap-fc0"
vm_config_template = "/etc/fc-runner/vm-config.json.template"
# jailer_path = "/usr/local/bin/jailer"
# jailer_uid = 1000
# jailer_gid = 1000
# jailer_chroot_base = "/srv/jailer"

[runner]
work_dir = "/var/lib/fc-runner/vms"
poll_interval_secs = 5
max_concurrent_jobs = 4
vm_timeout_secs = 3600

[network]
tap_device = "tap-fc0"
host_ip = "172.16.0.1"
guest_ip = "172.16.0.2"
cidr = "24"
dns = ["8.8.8.8", "1.1.1.1"]
# allowed_networks = ["github"]
```

## Sections

### `[github]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `token` | Yes | — | GitHub PAT with `repo` scope |
| `owner` | Yes | — | Repository owner (user or org) |
| `repo` | Yes | — | Repository name |
| `runner_group_id` | No | `1` | Runner group ID (1 = default) |
| `labels` | No | `["self-hosted", "linux", "firecracker"]` | Labels to match and advertise |

### `[firecracker]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `binary_path` | No | `/usr/local/bin/firecracker` | Path to the Firecracker binary |
| `kernel_path` | Yes | — | Path to the guest kernel (`vmlinux.bin`) |
| `rootfs_golden` | Yes | — | Path to the golden ext4 rootfs image |
| `vcpu_count` | No | `2` | Number of vCPUs per VM |
| `mem_size_mib` | No | `2048` | Memory in MiB per VM |
| `tap_interface` | No | `tap-fc0` | TAP device name for guest networking |
| `vm_config_template` | Yes | — | Path to `vm-config.json.template` |
| `jailer_path` | No | — | Path to the jailer binary. Enables chroot + seccomp-BPF + UID/GID drop |
| `jailer_uid` | If jailer | — | UID the jailer drops to before starting the VMM |
| `jailer_gid` | If jailer | — | GID the jailer drops to before starting the VMM |
| `jailer_chroot_base` | No | `/srv/jailer` | Base directory for jailer chroot environments |

### `[runner]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `work_dir` | No | `/var/lib/fc-runner/vms` | Directory for per-VM scratch files |
| `poll_interval_secs` | No | `5` | Seconds between GitHub API poll cycles |
| `max_concurrent_jobs` | No | `4` | Maximum VMs running simultaneously (prevents resource exhaustion) |
| `vm_timeout_secs` | No | `3600` | Maximum seconds a single VM is allowed to run before being killed |

### `[network]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `tap_device` | No | `tap-fc0` | TAP device name for guest networking |
| `host_ip` | No | `172.16.0.1` | Host-side IP address for the TAP interface |
| `guest_ip` | No | `172.16.0.2` | Guest-side IP address assigned inside the VM |
| `cidr` | No | `24` | CIDR prefix length for the TAP subnet |
| `dns` | No | `["8.8.8.8", "1.1.1.1"]` | DNS servers configured inside guest VMs |
| `allowed_networks` | No | `[]` (all allowed) | Outbound network allowlist. Use `"github"` to auto-fetch GitHub CIDRs. DNS servers are always allowed. |

### Network Allowlist

When `allowed_networks` is set, iptables FORWARD rules restrict guest VM outbound traffic to only the listed CIDRs. All other outbound traffic is dropped.

The special keyword `"github"` fetches all GitHub Actions, Git, API, and Web CIDRs from `https://api.github.com/meta` at startup. This ensures the runner can reach GitHub without manually tracking IP ranges.

```toml
[network]
# Only allow GitHub + internal network
allowed_networks = ["github", "10.0.0.0/8"]

# Allow everything (default when omitted)
# allowed_networks = []
```

DNS servers from the `dns` field are always added to the allowlist automatically.

## VM Config Template

The file at `vm_config_template` is a JSON file with placeholders that get replaced per-VM:

| Placeholder | Replaced With |
|-------------|---------------|
| `__KERNEL_PATH__` | `firecracker.kernel_path` |
| `__ROOTFS_PATH__` | Per-VM rootfs copy path |
| `__VCPU_COUNT__` | `firecracker.vcpu_count` |
| `__MEM_MIB__` | `firecracker.mem_size_mib` |
| `__TAP_IFACE__` | `firecracker.tap_interface` |
| `__LOG_PATH__` | Per-VM log file path |
| `__VM_ID__` | UUID-based VM identifier |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Controls log verbosity (e.g., `fc_runner=debug`, `fc_runner=trace`) |
