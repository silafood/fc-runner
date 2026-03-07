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

## Modules

### `main.rs`
Entry point. Loads configuration, initializes structured logging via `tracing`, sets up signal handlers (SIGTERM/SIGINT) for graceful shutdown, and launches the orchestrator.

### `config.rs`
Parses `/etc/fc-runner/config.toml` into typed structs using `serde` + `toml`. Validates all paths exist at load time. Redacts the GitHub token in Debug output. Supports both single `repo` and multi-repo `repos` fields (merged and deduplicated via `all_repos()`).

### `github.rs`
HTTP client wrapping `reqwest`. All API methods accept a `repo` parameter, allowing a single client to serve multiple repositories under the same owner. Communicates with three GitHub REST API endpoints:

| Method | Endpoint | Purpose |
|--------|----------|---------|
| GET | `/repos/{owner}/{repo}/actions/runs?status=queued` | List queued workflow runs |
| GET | `/repos/{owner}/{repo}/actions/runs/{id}/jobs?filter=queued` | List queued jobs for a run |
| POST | `/repos/{owner}/{repo}/actions/runners/generate-jit-config` | Generate a single-use JIT runner token |

All requests include `Authorization: Bearer <token>` and `X-GitHub-Api-Version: 2022-11-28`.

### `firecracker.rs`
Manages the full VM lifecycle through the `MicroVm` struct:

1. **copy_rootfs** — `cp --reflink=auto` of the golden ext4 image
2. **create_tap** — creates a per-VM TAP device (`tap-fc<slot>`) via `netlink.rs`
3. **inject_env** — loop-mount the copy, write `/etc/fc-runner-env` with JIT token and repo URL, write per-VM network config
4. **write_vm_config** — generate Firecracker VM config JSON programmatically (via `serde_json`)
5. **run** — spawn `firecracker --config-file <path> --no-api` (or via jailer) and wait for exit with timeout
6. **dump_guest_log** — mount rootfs read-only and extract `/var/log/runner.log` for debugging
7. **cleanup** — destroy TAP device, delete rootfs copy, config, socket, log, and jailer chroot files

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
Async poll loop using `tokio::time::interval`. Each cycle iterates over all configured repos:

1. For each repo, fetches queued runs from GitHub
2. For each run, fetches queued jobs
3. Filters jobs by label match
4. Deduplicates via `HashSet<u64>` of job IDs (shared across all repos)
5. Acquires a semaphore permit (bounded by `max_concurrent_jobs`)
6. Spawns a `tokio::spawn` task per new job with its repo context

The job ID is removed from the seen set after the task completes, allowing retry if the VM failed before the runner registered. Active job count is tracked for graceful shutdown. If one repo fails to poll, the others continue unaffected.

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
- JIT tokens are written into the ext4 image (not kernel cmdline) and deleted on VM teardown
- Token files inside the mounted rootfs are `chmod 0600`
- Config file permissions are checked at load time — rejects world-readable configs

### Path Safety
- All critical paths (`kernel_path`, `rootfs_golden`, `binary_path`) are validated against symlinks at config load time
- `path_str()` helper converts `PathBuf` to `&str` with descriptive errors instead of panicking

### VM Lifecycle
- `--no-api` disables the Firecracker management socket
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
