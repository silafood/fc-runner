# Configuration Reference

fc-runner is configured via a TOML file, default path `/etc/fc-runner/config.toml`.

Pass the config path via the `server` or `validate` subcommand:

```bash
# Start the server
fc-runner server --config /path/to/config.toml

# Validate without starting
fc-runner validate --config /path/to/config.toml
```

## Full Example

```toml
[github]
# Authentication: Use either a PAT (token) or GitHub App ([github.app]).
token = "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
owner = "your-org"
# Single repo
repo = "your-repo"
# Or multiple repos under the same owner
# repos = ["repo-one", "repo-two", "repo-three"]
# Organization-level runners (register at org level for cross-repo job pickup)
# organization = "your-org"
runner_group_id = 1
labels = ["self-hosted", "linux", "firecracker"]

# GitHub App authentication (alternative to PAT)
# [github.app]
# app_id = 12345
# installation_id = 67890
# private_key_path = "/etc/fc-runner/app-key.pem"

[firecracker]
binary_path = "/usr/local/bin/firecracker"
kernel_path = "/opt/fc-runner/vmlinux.bin"
rootfs_golden = "/opt/fc-runner/runner-rootfs-golden.ext4"
# image = "ghcr.io/silafood/fc-runner-image:567e784"  # public base image, or use your own
vcpu_count = 2
mem_size_mib = 2048
# boot_args = "console=ttyS0 reboot=k panic=1 pci=off fsck.mode=skip quiet loglevel=3"
# log_level = "Warning"
# cloud_img_url = "https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-amd64.img"
# jailer_path = "/usr/local/bin/jailer"
# jailer_uid = 1000
# jailer_gid = 1000
# jailer_chroot_base = "/var/lib/fc-runner/jailer"
# secret_injection = "mmds"
# vsock_enabled = true
# vsock_cid_base = 3

[runner]
work_dir = "/var/lib/fc-runner/vms"
poll_interval_secs = 5
max_concurrent_jobs = 4
vm_timeout_secs = 3600
# warm_pool_size = 2

# Named pools (alternative to warm_pool_size)
# [[pool]]
# name = "default"
# repos = ["repo-a", "repo-b"]
# min_ready = 2
# max_ready = 4
# vcpu_count = 4
# mem_size_mib = 4096

[network]
host_ip = "172.16.0.1"
guest_ip = "172.16.0.2"
cidr = "24"
dns = ["8.8.8.8", "1.1.1.1"]
# allowed_networks = ["github"]

[server]
# enabled = true
# listen_addr = "0.0.0.0:9090"
# api_key = "your-secret-key"
```

## Sections

### `[github]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `token` | One of `token`/`app` | — | GitHub PAT ([setup guide](setup.md#github-token-setup)) |
| `owner` | Yes | — | Repository owner (user or org) |
| `repo` | One of `repo`/`repos`/`organization` | — | Single repository name |
| `repos` | One of `repo`/`repos`/`organization` | `[]` | List of repos under the same owner |
| `organization` | No | — | Organization name for org-level runners. When set, runners register at the org level and can pick up jobs from any repo. |
| `runner_group_id` | No | `1` | Runner group ID (1 = default) |
| `labels` | No | `["self-hosted", "linux", "firecracker"]` | Labels to match and advertise |

> **Multi-repo:** Set `repos` to poll multiple repositories with a single fc-runner instance.
> Both `repo` and `repos` can be set — they are merged and deduplicated.
> All repos must share the same `owner`, `token`, `labels`, and `runner_group_id`.

> **Org-level runners:** Set `organization` to register runners at the GitHub organization level.
> Org-level runners can pick up jobs from any repo in the organization. The `repos` list is still
> used for polling queued jobs. JIT tokens and registration tokens are requested at the org level.

### `[github.app]`

GitHub App authentication as an alternative to PAT. Provides higher rate limits (5,000/hour per installation) and no token expiry management.

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `app_id` | Yes | — | GitHub App ID (from app settings page) |
| `installation_id` | Yes | — | Installation ID for the org/user where the app is installed |
| `private_key_path` | Yes | — | Path to the PEM private key file downloaded from GitHub |

When `[github.app]` is configured, `token` is optional. If both are set, App auth takes precedence.

**How it works:**
1. fc-runner generates a JWT signed with the App's private key (RS256, 10-minute TTL)
2. Exchanges the JWT for an installation access token via `POST /app/installations/{id}/access_tokens`
3. Caches the installation token for 55 minutes (tokens are valid for 60 minutes)
4. Automatically refreshes before expiry

**Setup:**
1. Create a GitHub App at `https://github.com/settings/apps/new`
2. Grant **Repository permissions**: Actions (R/W), Administration (R/W), Metadata (Read)
3. Install the App on the org/user that owns the repos
4. Download the private key PEM file
5. Note the App ID and Installation ID

```toml
[github]
owner = "your-org"
repos = ["repo-a", "repo-b"]

[github.app]
app_id = 12345
installation_id = 67890
private_key_path = "/etc/fc-runner/app-key.pem"
```

### `[firecracker]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `binary_path` | No | `/usr/local/bin/firecracker` | Path to the Firecracker binary |
| `kernel_path` | Yes | — | Path to the guest kernel (`vmlinux.bin`). Auto-downloaded if missing. |
| `rootfs_golden` | Yes | — | Path to the golden ext4 rootfs image. Auto-built if missing. |
| `image` | No | — | OCI image reference (e.g., `ghcr.io/org/image:latest`). When set, the image is pulled, converted to ext4, and cached at `rootfs_golden` path. Takes precedence over the cloud image build pipeline. |
| `vcpu_count` | No | `2` | Number of vCPUs per VM |
| `mem_size_mib` | No | `2048` | Memory in MiB per VM |
| `boot_args` | No | `console=ttyS0 reboot=k panic=1 pci=off fsck.mode=skip quiet loglevel=3` | Kernel boot arguments |
| `log_level` | No | `Warning` | Firecracker log level: `Error`, `Warning`, `Info`, `Debug` |
| `cloud_img_url` | No | Ubuntu 24.04 minimal | URL for the cloud image used to build the golden rootfs |
| `jailer_path` | No | — | Path to the jailer binary. Enables chroot + seccomp-BPF + UID/GID drop |
| `jailer_uid` | If jailer | — | UID the jailer drops to before starting the VMM |
| `jailer_gid` | If jailer | — | GID the jailer drops to before starting the VMM |
| `jailer_chroot_base` | No | `/var/lib/fc-runner/jailer` | Base directory for jailer chroot environments |
| `secret_injection` | No | `mmds` | Secret injection method: `mmds` (Firecracker metadata service) or `mount` (legacy loop-mount) |
| `vsock_enabled` | No | `false` | Enable virtio-vsock device for guest agent communication |
| `vsock_cid_base` | No | `3` | Base CID for VSOCK devices. Each VM gets CID = `vsock_cid_base + slot`. CIDs 0-2 are reserved. |

#### Secret Injection Methods

**`mmds`** (default) — Uses Firecracker's built-in MicroVM Metadata Service:
- Secrets are served at `169.254.169.254` inside the guest (like cloud instance metadata)
- Uses MMDS V2 with session tokens for security
- No loop-mount needed for secrets (only for network config)
- Secrets never written to disk — injected via the Firecracker API socket
- Guest uses `guest_configs/fetch-mmds-env.sh` to retrieve secrets at boot

**`mount`** (legacy) — Loop-mounts the rootfs copy:
- Secrets are written to `/etc/fc-runner-env` inside the rootfs
- File is `chmod 0600` and deleted on VM teardown
- Requires loop devices (can exhaust `/dev/loop*` under high concurrency)
- VM runs with `--no-api` (no API socket)

#### VSOCK

When `vsock_enabled = true`, each VM gets a virtio-vsock device. The host spawns a VSOCK listener per VM that reads NDJSON messages from the guest agent on port 1024. This enables structured host-guest communication without network overhead.

#### OCI Image Mode

When `image` is set, fc-runner pulls the specified OCI container image from the registry, extracts its layers onto an ext4 filesystem, and uses it as the golden rootfs. This replaces the cloud image build pipeline and lets you define VM images as standard Dockerfiles.

```toml
[firecracker]
image = "ghcr.io/your-org/fc-runner-image:latest"
rootfs_golden = "/opt/fc-runner/runner-rootfs-golden.ext4"
```

**How it works:**
1. Pulls the image manifest and checks digest against cached version
2. If changed (or first pull): creates a blank ext4 image, mounts it, extracts layers in order
3. Handles OCI whiteout files (`.wh.*` deletions, `.wh..wh..opq` opaque directory replacements)
4. Installs the fc-runner agent binary if not already present in the image
5. Saves the digest for future cache checks

**Building a custom image:**
```bash
# Build using the provided sample Dockerfile
docker build -t ghcr.io/your-org/fc-runner-image:latest -f Dockerfile.runner .
docker push ghcr.io/your-org/fc-runner-image:latest
```

The sample `Dockerfile.runner` includes Ubuntu 24.04, build tools, Rust, GitHub Actions runner, and systemd networking. Customize it to add your own dependencies.

**Notes:**
- The fc-runner agent binary is automatically installed if not found in the image
- `rootfs_golden` is still required — it specifies where the converted ext4 image is cached
- When `image` is not set, the existing cloud image pipeline is used (backward compatible)
- Anonymous registry access is used by default (works with public images on GHCR, Docker Hub, etc.)

### `[runner]`

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `work_dir` | No | `/var/lib/fc-runner/vms` | Directory for per-VM scratch files |
| `poll_interval_secs` | No | `5` | Seconds between GitHub API poll cycles |
| `max_concurrent_jobs` | No | `4` | Maximum VMs running simultaneously (prevents resource exhaustion) |
| `vm_timeout_secs` | No | `3600` | Maximum seconds a single VM is allowed to run before being killed |
| `warm_pool_size` | No | `0` | Number of pre-registered idle runners. When > 0, uses registration tokens instead of JIT. |

### `[[pool]]`

Named VM pools provide fine-grained control over scaling and resource allocation. When `[[pool]]` sections are configured, they replace the flat `warm_pool_size` setting.

Each pool maintains its own set of pre-registered runners with independent configuration.

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `name` | Yes | — | Pool name (used in logs) |
| `repos` | Yes | — | List of repos this pool serves |
| `min_ready` | No | `1` | Minimum number of idle VMs to keep ready |
| `max_ready` | No | `4` | Maximum total VMs in this pool (idle + active) |
| `vcpu_count` | No | `[firecracker].vcpu_count` | Override vCPU count for this pool |
| `mem_size_mib` | No | `[firecracker].mem_size_mib` | Override memory for this pool |

Slots are distributed across pools based on each pool's `max_ready`. If the total `max_ready` across all pools exceeds `max_concurrent_jobs`, a warning is logged and some pools may not receive all requested slots.

```toml
# Lightweight pool for CI jobs
[[pool]]
name = "ci"
repos = ["app", "lib"]
min_ready = 2
max_ready = 4

# Heavy pool for integration tests
[[pool]]
name = "integration"
repos = ["app"]
min_ready = 1
max_ready = 2
vcpu_count = 8
mem_size_mib = 8192
```

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

### `[server]`

HTTP server for Prometheus metrics, health checks, and a management REST API.

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `enabled` | No | `true` | Set to `false` to disable the HTTP server entirely |
| `listen_addr` | No | `0.0.0.0:9090` | Listen address for the HTTP server |
| `api_key` | No | — | API key for authenticated management endpoints. When set, `X-Api-Key` header is required for `/api/v1/vms` and `DELETE /api/v1/vms/{id}`. |

**Endpoints:**

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| GET | `/metrics` | No | Prometheus metrics in text exposition format |
| GET | `/healthz` | No | Health check (returns `ok`) |
| GET | `/api/v1/status` | No | JSON: version, uptime, operating mode, active VM count |
| GET | `/api/v1/vms` | API key | JSON array of active VMs (vm_id, job_id, repo, slot, started_at) |
| DELETE | `/api/v1/vms/{id}` | API key | Request termination of a specific VM |
| GET | `/api/v1/pools` | API key | List all pools with status (name, repos, min/max ready, active count, paused) |
| GET | `/api/v1/pools/{name}` | API key | Get single pool detail |
| POST | `/api/v1/pools/{name}/scale` | API key | Scale pool: body `{"min_ready": N, "max_ready": M}` |
| POST | `/api/v1/pools/{name}/pause` | API key | Pause pool (stops spawning new VMs, active VMs continue) |
| POST | `/api/v1/pools/{name}/resume` | API key | Resume a paused pool |

**Prometheus metrics example:**
```bash
curl -s http://localhost:9090/metrics
```

**Management API example:**
```bash
# Without API key (open access)
curl -s http://localhost:9090/api/v1/vms | jq .

# With API key
curl -s -H "X-Api-Key: your-secret-key" http://localhost:9090/api/v1/vms | jq .

# Pool management
curl -s -H "X-Api-Key: your-secret-key" http://localhost:9090/api/v1/pools | jq .
curl -s -X POST -H "X-Api-Key: your-secret-key" -H "Content-Type: application/json" \
  -d '{"min_ready": 3, "max_ready": 6}' http://localhost:9090/api/v1/pools/default/scale
curl -s -X POST -H "X-Api-Key: your-secret-key" http://localhost:9090/api/v1/pools/default/pause
curl -s -X POST -H "X-Api-Key: your-secret-key" http://localhost:9090/api/v1/pools/default/resume
```

### CLI Subcommands

fc-runner provides CLI commands that call the management API:

```bash
# List running VMs
fc-runner ps --endpoint http://localhost:9090

# Pool management
fc-runner pools list --endpoint http://localhost:9090
fc-runner pools scale default --min-ready 3 --endpoint http://localhost:9090
fc-runner pools pause default --endpoint http://localhost:9090
fc-runner pools resume default --endpoint http://localhost:9090

# Stream VM logs
fc-runner logs --vm-id <id> --endpoint http://localhost:9090
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Controls log verbosity (e.g., `fc_runner=debug`, `fc_runner=trace`) |
