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
# Single repo
repo = "your-repo"
# Or multiple repos under the same owner
# repos = ["repo-one", "repo-two", "repo-three"]
runner_group_id = 1
labels = ["self-hosted", "linux", "firecracker"]

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/runner-rootfs-golden.ext4"
vcpu_count = 2
mem_size_mib = 2048
# boot_args = "console=ttyS0 reboot=k panic=1 pci=off fsck.mode=skip quiet loglevel=3"
# log_level = "Warning"
# cloud_img_url = "https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-amd64.img"
# jailer_path = "/usr/local/bin/jailer"
# jailer_uid = 1000
# jailer_gid = 1000
# jailer_chroot_base = "/srv/jailer"

[runner]
work_dir = "/var/lib/fc-runner/vms"
poll_interval_secs = 5
max_concurrent_jobs = 4
vm_timeout_secs = 3600
# warm_pool_size = 2

[network]
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
| `token` | Yes | — | GitHub PAT ([setup guide](setup.md#github-token-setup)) |
| `owner` | Yes | — | Repository owner (user or org) |
| `repo` | One of `repo`/`repos` | — | Single repository name |
| `repos` | One of `repo`/`repos` | `[]` | List of repos under the same owner |
| `runner_group_id` | No | `1` | Runner group ID (1 = default) |
| `labels` | No | `["self-hosted", "linux", "firecracker"]` | Labels to match and advertise |

> **Multi-repo:** Set `repos` to poll multiple repositories with a single fc-runner instance.
> Both `repo` and `repos` can be set — they are merged and deduplicated.
> All repos must share the same `owner`, `token`, `labels`, and `runner_group_id`.

### `[firecracker]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `binary_path` | No | `/usr/local/bin/firecracker` | Path to the Firecracker binary |
| `kernel_path` | Yes | — | Path to the guest kernel (`vmlinux.bin`). Auto-downloaded if missing. |
| `rootfs_golden` | Yes | — | Path to the golden ext4 rootfs image. Auto-built if missing. |
| `vcpu_count` | No | `2` | Number of vCPUs per VM |
| `mem_size_mib` | No | `2048` | Memory in MiB per VM |
| `boot_args` | No | `console=ttyS0 reboot=k panic=1 pci=off fsck.mode=skip quiet loglevel=3` | Kernel boot arguments |
| `log_level` | No | `Warning` | Firecracker log level: `Error`, `Warning`, `Info`, `Debug` |
| `cloud_img_url` | No | Ubuntu 24.04 minimal | URL for the cloud image used to build the golden rootfs |
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
| `warm_pool_size` | No | `0` | Number of pre-registered idle runners. When > 0, uses registration tokens instead of JIT. |

### `[network]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `host_ip` | No | `172.16.0.1` | Base host-side IP address (per-VM IPs are derived from slot: `172.16.<slot>.1`) |
| `guest_ip` | No | `172.16.0.2` | Base guest-side IP address (per-VM: `172.16.<slot>.2`) |
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

### Per-VM Networking

Each VM gets its own TAP device and unique subnet derived from its slot number:

| Slot | TAP Device | Host IP | Guest IP | Guest MAC |
|------|-----------|---------|----------|-----------|
| 0 | `tap-fc0` | `172.16.0.1` | `172.16.0.2` | `06:00:AC:10:00:02` |
| 1 | `tap-fc1` | `172.16.1.1` | `172.16.1.2` | `06:00:AC:10:01:02` |
| 2 | `tap-fc2` | `172.16.2.1` | `172.16.2.2` | `06:00:AC:10:02:02` |

TAP devices are created and destroyed automatically per VM. No manual TAP configuration is needed.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Controls log verbosity (e.g., `fc_runner=debug`, `fc_runner=trace`) |
