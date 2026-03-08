# Architecture

## Overview

fc-runner follows a poll-dispatch-cleanup lifecycle:

```
GitHub API ──poll──▶ Orchestrator ──spawn──▶ MicroVM (Firecracker)
  (per repo)            │                        │
                   dedup via HashSet         COW rootfs + JIT token
                        │                        │
                   tokio::spawn              run job → exit → cleanup
```

## Operating Modes

fc-runner supports three operating modes, selected automatically based on configuration:

| Mode | Config | Description |
|------|--------|-------------|
| **Reactive (JIT)** | Default | Polls GitHub for queued jobs, boots a VM per job on demand |
| **Warm pool** | `warm_pool_size > 0` | Pre-registers N idle runners that wait for GitHub to assign jobs |
| **Named pools** | `[[pool]]` sections | Multiple named pools with per-pool repos, replica counts, and resource overrides |

## Modules

### `main.rs`
Entry point with clap CLI dispatch. Parses subcommands (`server`, `agent`, `validate`, `ps`, `pools`, `logs`) and delegates to the appropriate handler. The `server` subcommand loads configuration, initializes structured logging via `tracing`, sets up signal handlers (SIGTERM/SIGINT) for graceful shutdown, creates the shared `ServerState`, spawns the HTTP server, and launches the orchestrator.

### `cli.rs`
Defines the CLI interface using `clap` derive macros. Contains the `Cli` struct, `Commands` enum (Server, Agent, Validate, Ps, Pools, Logs), and `PoolAction` enum (List, Scale, Pause, Resume). Each subcommand has typed arguments with defaults.

### `api_client.rs`
HTTP client for CLI-to-server management API communication. Used by the `ps`, `pools`, and `logs` subcommands to query the running fc-runner server. Methods include `status()`, `list_vms()`, `list_pools()`, `get_pool()`, `scale_pool()`, `pause_pool()`, and `resume_pool()`.

### `agent.rs`
Guest agent that runs inside Firecracker VMs via `fc-runner agent`. Reads MMDS V2 metadata (token acquisition via PUT, then GET `/fc-runner`), starts the GitHub Actions runner with the JIT config, sets the VM hostname, and reports state to the host via VSOCK NDJSON messages (Ready, JobStarted, JobCompleted). Optionally shuts down the VM after the runner exits. Uses conditional compilation (`#[cfg(target_os = "linux")]`) for VSOCK-dependent code.

### `config.rs`
Parses `/etc/fc-runner/config.toml` into typed structs using `serde` + `toml`. Validates all paths exist at load time. Redacts the GitHub token in Debug output. Supports both single `repo` and multi-repo `repos` fields (merged and deduplicated via `all_repos()`).

Key config structs:
- `AppConfig` — top-level config with `github`, `firecracker`, `runner`, `network`, `server`, and `pool` sections
- `GitHubConfig` — PAT token (optional) + `GitHubAppConfig` for App auth
- `FirecrackerConfig` — VM resources, secret injection mode (`mmds`/`mount`), VSOCK settings
- `ServerConfig` — HTTP server listen address, enabled flag, optional API key
- `PoolConfig` — named pool with repos, min/max ready counts, resource overrides

### `github.rs`
HTTP client wrapping `reqwest`. Supports two authentication methods:

| Auth Method | Config | Description |
|-------------|--------|-------------|
| **PAT** | `github.token` | Personal access token, sent as `Authorization: Bearer <token>` |
| **GitHub App** | `[github.app]` | JWT-based auth with installation token caching (55-min TTL, auto-refresh) |

The `AuthProvider` enum manages both methods. App auth generates a JWT (RS256), exchanges it for an installation access token via `POST /app/installations/{id}/access_tokens`, and caches the result in a `RwLock` with automatic refresh before expiry.

All API methods accept a `repo` parameter, allowing a single client to serve multiple repositories under the same owner. When `github.organization` is set, the client operates in org mode — JIT configs and registration tokens are requested at the organization level instead of per-repo.

**Repo-level endpoints:**

| Method | Endpoint | Purpose |
|--------|----------|---------|
| GET | `/repos/{owner}/{repo}/actions/runs?status=queued` | List queued workflow runs |
| GET | `/repos/{owner}/{repo}/actions/runs/{id}/jobs?filter=queued` | List queued jobs for a run |
| POST | `/repos/{owner}/{repo}/actions/runners/generate-jit-config` | Generate a single-use JIT runner token |
| POST | `/repos/{owner}/{repo}/actions/runners/registration-token` | Generate a registration token (warm pool mode) |
| DELETE | `/repos/{owner}/{repo}/actions/runners/{id}` | Remove offline runners after VM exit |

**Org-level endpoints** (when `github.organization` is set):

| Method | Endpoint | Purpose |
|--------|----------|---------|
| POST | `/orgs/{org}/actions/runners/generate-jit-config` | Generate an org-level JIT runner token |
| POST | `/orgs/{org}/actions/runners/registration-token` | Generate an org-level registration token |
| GET | `/orgs/{org}/actions/runners` | List org runners (for offline cleanup) |
| DELETE | `/orgs/{org}/actions/runners/{id}` | Remove offline org-level runners |

All requests include `Authorization: Bearer <token>` and `X-GitHub-Api-Version: 2022-11-28`.

### `firecracker.rs`
Manages the full VM lifecycle through the `MicroVm` struct. Supports two secret injection modes:

**MMDS mode** (default, `secret_injection = "mmds"`):
1. **copy_rootfs** — `cp --reflink=auto` of the golden ext4 image
2. **create_tap** — creates a per-VM TAP device (`tap-fc<slot>`) via `netlink.rs`
3. **inject_network_config** — loop-mount the copy, write per-VM network config (but not secrets)
4. **write_vm_config** — generate VM config JSON with MMDS V2 enabled
5. **run_with_api** — spawn `firecracker --config-file <path> --api-sock <sock>`, then PUT secrets to MMDS via Unix socket HTTP
6. **dump_guest_log** / **cleanup**

**Mount mode** (legacy, `secret_injection = "mount"`):
1. **copy_rootfs** + **create_tap** (same as above)
2. **inject_env_mount** — loop-mount, write `/etc/fc-runner-env` with JIT token + repo URL + network config
3. **write_vm_config** — generate VM config JSON (no MMDS)
4. **run_no_api** — spawn `firecracker --config-file <path> --no-api`
5. **dump_guest_log** / **cleanup**

When VSOCK is enabled (`vsock_enabled = true`), the VM config includes a `vsock` device with `guest_cid = vsock_cid_base + slot`.

Cleanup runs unconditionally, even if earlier steps fail.

### `netlink.rs`
Pure-Rust TAP device management module. On Linux, uses `ioctl(TUNSETIFF)` via `nix` crate for TAP creation and `rtnetlink` crate for IP assignment and link state management. On non-Linux platforms, falls back to `ip` commands for development/testing.

Key functions:
- `create_tap(name)` — creates a persistent TAP device via ioctl
- `delete_link(name)` — removes a network interface
- `add_address_v4(name, addr, prefix)` — assigns an IPv4 address
- `set_link_up(name)` — brings an interface up
- `link_exists(name)` — checks if a link exists

### `setup.rs`
Auto-provisioning module that ensures all VM prerequisites are in place at startup:

1. **preflight_kvm** — verifies `/dev/kvm` exists and is accessible
2. **resolve_allowed_networks** — expands the `"github"` keyword into actual CIDRs from `api.github.com/meta`
3. **ensure_kernel** — downloads the guest kernel if missing (from GitHub releases)
4. **ensure_golden_rootfs** — builds the golden rootfs if missing:
   - Downloads Ubuntu 24.04 minimal cloud image (qcow2)
   - Converts qcow2 to raw via `qcow2-rs` (pure Rust)
   - Finds ext4 partition via `bootsector` crate + magic byte check (0xEF53)
   - Extracts and expands to a standalone ext4 image
   - Mounts, installs packages via chroot (git, curl, jq, actions-runner)
   - Creates runner user, entrypoint script, systemd service units
   - Shrinks to minimum size + headroom via `resize2fs`
5. **ensure_network** — enables IP forwarding, configures iptables NAT/FORWARD rules
6. **ensure_apparmor** — loads and enforces AppArmor profiles for fc-runner and Firecracker

### `orchestrator.rs`
Async poll loop using `tokio::time::interval`. Supports three modes:

**Reactive (JIT) mode** — default. Each cycle iterates over all configured repos:
1. For each repo, fetches queued runs from GitHub
2. For each run, fetches queued jobs
3. Filters jobs by label match
4. Deduplicates via `HashSet<u64>` of job IDs (shared across all repos)
5. Acquires a semaphore permit (bounded by `max_concurrent_jobs`)
6. Spawns a `tokio::spawn` task per new job with its repo context
7. Registers the VM in `ServerState` for management API visibility
8. Records Prometheus metrics (dispatch count, boot duration, active jobs)

**Warm pool mode** — `warm_pool_size > 0`. Pre-registers idle runners:
1. Spawns `warm_pool_size` VMs distributed across repos round-robin
2. Each VM uses a registration token (not JIT) and waits for GitHub to assign a job
3. When a VM finishes, a replacement is spawned automatically after a brief delay

**Named pools mode** — `[[pool]]` sections configured. Delegates to `PoolManager`:
1. Distributes slots across pools proportionally
2. Each pool runs independently with its own `PoolManager` instance

The job ID is removed from the seen set after the task completes, allowing retry if the VM failed before the runner registered. Active job count is tracked for graceful shutdown. If one repo fails to poll, the others continue unaffected.

### `metrics.rs`
Prometheus metrics registry using the `prometheus` crate with `Lazy` static initialization. All metrics are registered in a custom `Registry` and gathered via `TextEncoder`.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `fc_jobs_dispatched_total` | Counter | `repo` | Total jobs dispatched |
| `fc_jobs_completed_total` | Counter | `repo` | Total jobs completed successfully |
| `fc_jobs_failed_total` | Counter | `repo` | Total jobs that failed |
| `fc_jobs_active` | Gauge | — | Currently running VMs |
| `fc_vm_boot_duration_seconds` | Histogram | `repo` | VM boot + execution duration |
| `fc_github_api_calls_total` | Counter | `endpoint` | GitHub API calls |
| `fc_github_rate_limit_remaining` | Gauge | — | Remaining GitHub API rate limit |
| `fc_pool_slots_available` | Gauge | — | Available concurrency slots |
| `fc_poll_cycles_total` | Counter | `result` | Poll cycle outcomes (ok/error) |
| `fc_uptime_seconds` | Gauge | `version` | Process uptime |

### `server.rs`
Axum HTTP server providing Prometheus metrics, health checks, and a management API. Shared state is managed via `Arc<ServerState>`.

**Endpoints:**

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| GET | `/metrics` | No | Prometheus metrics in text format |
| GET | `/healthz` | No | Health check (returns "ok") |
| GET | `/api/v1/status` | No | JSON status (version, uptime, mode, active VM count) |
| GET | `/api/v1/vms` | API key | List active VMs with job ID, repo, slot, start time |
| DELETE | `/api/v1/vms/{id}` | API key | Request VM termination |
| GET | `/api/v1/pools` | API key | List all pools with status |
| GET | `/api/v1/pools/{name}` | API key | Get single pool detail |
| POST | `/api/v1/pools/{name}/scale` | API key | Scale pool (body: `{"min_ready": N, "max_ready": M}`) |
| POST | `/api/v1/pools/{name}/pause` | API key | Pause pool (stops spawning new VMs) |
| POST | `/api/v1/pools/{name}/resume` | API key | Resume a paused pool |

When `server.api_key` is configured, management endpoints require an `X-Api-Key` header. Health and metrics endpoints are always unauthenticated.

### `pool.rs`
Named VM pool manager. Each `PoolManager` maintains a set of warm VMs for its assigned repos:

- Keeps `min_ready` idle VMs at all times
- Limits total VMs (idle + active) to `max_ready`
- Supports per-pool `vcpu_count` and `mem_size_mib` overrides
- Automatically replaces VMs after they complete a job
- Uses `mpsc` channels for slot return signaling
- Runtime management via atomic operations: `pause()`, `resume()`, `scale()`, `status()`
- Paused pools stop spawning new VMs but let active VMs finish
- Pool managers are registered with `ServerState` for REST API access

### `vsock.rs`
Host-side VSOCK listener for guest agent communication. When `vsock_enabled = true`, a listener is spawned per VM before launch and aborted on VM exit.

**Protocol:** NDJSON over VSOCK port 1024.

| Message type | Fields | Description |
|-------------|--------|-------------|
| `ready` | `timestamp` | Guest agent initialized |
| `job_started` | `job_id` | Runner picked up a job |
| `log` | `line` | Structured log line from guest |
| `job_completed` | `exit_code` | Job finished |
| `heartbeat` | — | Periodic liveness signal |

Linux-only (`tokio-vsock` crate); stub implementation on other platforms.

## Auto-Provisioning

At first startup, fc-runner automatically provisions all required assets:

```
ensure_vm_assets()
  ├─ preflight_kvm()           → verify /dev/kvm
  ├─ resolve_allowed_networks() → expand "github" → CIDRs
  ├─ ensure_kernel()           → download vmlinux if missing
  ├─ ensure_golden_rootfs()    → build rootfs from cloud image if missing
  │   ├─ download qcow2 cloud image
  │   ├─ convert qcow2 → raw (pure Rust, qcow2-rs)
  │   ├─ find ext4 partition (bootsector crate)
  │   ├─ extract + expand ext4 image
  │   ├─ mount + chroot: install packages, runner, entrypoint
  │   └─ shrink to minimum size (e2fsck + resize2fs)
  ├─ ensure_network()          → ip_forward + iptables NAT
  └─ ensure_apparmor()         → load profiles if available
```

To force a rebuild, delete the golden rootfs and restart:
```bash
sudo rm /opt/fc-runner/runner-rootfs-golden.ext4
sudo systemctl restart fc-runner
```

## Concurrency

Each job runs in its own tokio task. Concurrency is bounded by a `tokio::sync::Semaphore` initialized from `runner.max_concurrent_jobs` (default: 4). This prevents resource exhaustion on the host when many jobs queue simultaneously.

In named pool mode, slots are distributed across pools proportionally based on each pool's `max_ready` setting.

## Per-VM Networking

Each VM gets its own TAP device and unique subnet:

```
Host (172.16.<slot>.1/24) ←→ tap-fc<slot> ←→ Guest eth0 (172.16.<slot>.2/24)
      ↓ iptables MASQUERADE + TCP MSS clamping
    Internet / GitHub API
```

- TAP devices are created via `ioctl(TUNSETIFF)` with `TUNSETPERSIST` so they survive fd close
- IP addresses are assigned via `rtnetlink` (pure Rust netlink API)
- Each VM gets a unique MAC address: `06:00:AC:10:<slot_hex>:02`
- Guest network config is written via systemd-networkd unit files injected into the rootfs

## Secret Injection

fc-runner supports two methods for injecting secrets (JIT token, repo URL) into VMs:

### MMDS (default)
Uses Firecracker's built-in Metadata Data Store at `169.254.169.254`:
1. VM starts with `--api-sock` (API socket enabled)
2. Host PUTs metadata as JSON to the MMDS via the Unix socket
3. Guest fetches secrets from `http://169.254.169.254/latest/meta-data/` using a V2 session token
4. No loop-mount needed for secret injection (only for network config)

**Advantages:** No root required for secret injection, no loop device exhaustion risk, secrets never written to disk.

### Mount (legacy)
Injects secrets by loop-mounting the rootfs copy:
1. Loop-mount the COW rootfs copy
2. Write secrets to `/etc/fc-runner-env`
3. Unmount
4. VM starts with `--no-api`

## VM Timeout

Each VM execution is wrapped in `tokio::time::timeout` using `runner.vm_timeout_secs` (default: 3600). If a VM exceeds this limit, it is killed and the job fails with a timeout error.

## Graceful Shutdown

On SIGTERM/SIGINT, the orchestrator:

1. Stops the poll loop (no new jobs dispatched)
2. Waits up to 5 minutes for active VMs to finish
3. Logs the count of still-running jobs if the timeout expires

Active job count is tracked via `Arc<Mutex<usize>>`.

## Security

### Secret Handling
- GitHub PAT stored as `secrecy::SecretString` — zeroized on drop, redacted in Debug/logs
- GitHub App private keys read from disk at startup, never logged
- JIT tokens injected via MMDS (in-memory, never on disk) or written to ext4 with `chmod 0600`
- Token files inside the mounted rootfs are deleted on VM teardown
- Config file permissions are checked at load time — rejects world-readable configs

### Path Safety
- All critical paths (`kernel_path`, `rootfs_golden`, `binary_path`) are validated against symlinks at config load time
- `path_str()` helper converts `PathBuf` to `&str` with descriptive errors instead of panicking

### VM Lifecycle
- `--no-api` disables the Firecracker management socket (mount mode)
- `--api-sock` enables it only for MMDS injection, then the socket is cleaned up
- `umount_with_retry()`: 3 attempts with 200ms delay, then lazy umount fallback to prevent leaked mounts
- Guest log dump after every VM exit (mount read-only, extract `/var/log/runner.log`)
- Cleanup runs unconditionally, even on failure

### Jailer Integration
- When `firecracker.jailer_path` is set in config, VMs launch inside a jailer chroot
- Jailer applies: chroot isolation, seccomp-BPF syscall filtering, UID/GID drop to unprivileged user
- Requires `jailer_uid` and `jailer_gid` — validated at config load time
- Jailer chroot directories are cleaned up automatically after each VM exits

### Network Allowlist
- When `network.allowed_networks` is configured, iptables FORWARD rules restrict guest outbound traffic to only listed CIDRs
- The `"github"` keyword auto-fetches GitHub Actions/Git/API/Web CIDRs from `https://api.github.com/meta` at startup
- DNS servers are always added to the allowlist automatically
- Unmatched outbound traffic is dropped via a trailing DROP rule
- When the list is empty (default), all outbound traffic is permitted

### Rate Limiting
- GitHub API rate-limit headers (`x-ratelimit-remaining`) are parsed after every response
- Warning at < 100 remaining requests, 60-second backoff at < 10
- GitHub App auth provides higher rate limits (5,000/hour per installation vs per PAT)

### Host Hardening
- systemd service runs with `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=true`, `MemoryDenyWriteExecute`, and restricted capabilities
- Work directory created with `0700` permissions

### AppArmor

fc-runner ships with AppArmor profiles for both binaries:

| Profile | Binary | Restrictions |
|---------|--------|-------------|
| `usr.local.bin.firecracker` | Firecracker VMM | Read-only kernel, r/w only per-VM rootfs copies in work dir, KVM and TAP device access, no network, no arbitrary filesystem access |
| `usr.local.bin.fc-runner` | Orchestrator | Config read-only, VM work dir r/w with link, mount/umount for rootfs injection, chroot with full capabilities for rootfs provisioning, network admin for TAP/NAT, child exec only Firecracker/jailer |

Profiles are installed by `install.sh` to `/etc/apparmor.d/` and enforced automatically at startup by `setup.rs`. If AppArmor or `apparmor-utils` is not available, fc-runner continues without enforcement and logs a warning.

**Manual management:**
```bash
# Check enforcement status
sudo aa-status | grep -E '(firecracker|fc-runner)'

# Reload after profile update
sudo apparmor_parser -r /etc/apparmor.d/usr.local.bin.fc-runner

# Switch to complain mode (log violations without blocking)
sudo aa-complain /etc/apparmor.d/usr.local.bin.firecracker

# Re-enforce
sudo aa-enforce /etc/apparmor.d/usr.local.bin.firecracker

# Check for denied operations
sudo dmesg | grep DENIED | tail -20
```
