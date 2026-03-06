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

[runner]
work_dir = "/var/lib/fc-runner/vms"
poll_interval_secs = 5
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

### `[runner]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `work_dir` | No | `/var/lib/fc-runner/vms` | Directory for per-VM scratch files |
| `poll_interval_secs` | No | `5` | Seconds between GitHub API poll cycles |

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
