# Architecture

## Overview

fc-runner follows a poll-dispatch-cleanup lifecycle:

```
GitHub API ──poll──▶ Orchestrator ──spawn──▶ MicroVM (Firecracker)
                        │                        │
                   dedup via HashSet         COW rootfs + JIT token
                        │                        │
                   tokio::spawn              run job → exit → cleanup
```

## Modules

### `main.rs`
Entry point. Loads configuration, initializes structured logging via `tracing`, sets up signal handlers (SIGTERM/SIGINT) for graceful shutdown, and launches the orchestrator.

### `config.rs`
Parses `/etc/fc-runner/config.toml` into typed structs using `serde` + `toml`. Validates all paths exist at load time. Redacts the GitHub token in Debug output.

### `github.rs`
HTTP client wrapping `reqwest`. Communicates with three GitHub REST API endpoints:

| Method | Endpoint | Purpose |
|--------|----------|---------|
| GET | `/repos/{owner}/{repo}/actions/runs?status=queued` | List queued workflow runs |
| GET | `/repos/{owner}/{repo}/actions/runs/{id}/jobs?filter=queued` | List queued jobs for a run |
| POST | `/repos/{owner}/{repo}/actions/runners/generate-jit-config` | Generate a single-use JIT runner token |

All requests include `Authorization: Bearer <token>` and `X-GitHub-Api-Version: 2022-11-28`.

### `firecracker.rs`
Manages the full VM lifecycle through the `MicroVm` struct:

1. **copy_rootfs** — `cp --reflink=auto` of the golden ext4 image
2. **inject_env** — loop-mount the copy, write `/run/fc-runner-env` with JIT token and repo URL
3. **write_vm_config** — render `vm-config.json.template` with per-VM values
4. **run** — spawn `firecracker --config-file <path> --no-api` and wait for exit
5. **cleanup** — delete rootfs copy, config, socket, and log files

Cleanup runs unconditionally, even if earlier steps fail.

### `orchestrator.rs`
Async poll loop using `tokio::time::interval`. Each cycle:

1. Fetches queued runs from GitHub
2. For each run, fetches queued jobs
3. Filters jobs by label match
4. Deduplicates via `HashSet<u64>` of job IDs
5. Acquires a semaphore permit (bounded by `max_concurrent_jobs`)
6. Spawns a `tokio::spawn` task per new job

The job ID is removed from the seen set after the task completes, allowing retry if the VM failed before the runner registered. Active job count is tracked for graceful shutdown.

## Concurrency

Each job runs in its own tokio task. Concurrency is bounded by a `tokio::sync::Semaphore` initialized from `runner.max_concurrent_jobs` (default: 4). This prevents resource exhaustion on the host when many jobs queue simultaneously.

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
- Config file permissions are checked at load time — warns if world-readable

### Path Safety
- All critical paths (`kernel_path`, `rootfs_golden`, `binary_path`, `vm_config_template`) are validated against symlinks at config load time
- `path_str()` helper converts `PathBuf` to `&str` with descriptive errors instead of panicking

### VM Lifecycle
- `--no-api` disables the Firecracker management socket
- Mount TOCTOU protection: `mountpoint -q` verification runs after every mount
- `umount_with_retry()`: 3 attempts with 200ms delay, then lazy umount fallback to prevent leaked mounts
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
| `usr.local.bin.fc-runner` | Orchestrator | Config read-only, VM work dir r/w, mount/umount for rootfs injection, network admin for TAP/NAT, child exec only Firecracker/jailer |

Profiles are installed by `install.sh` to `/etc/apparmor.d/` and enforced automatically at startup by `setup.rs`. If AppArmor or `apparmor-utils` is not available, fc-runner continues without enforcement and logs a warning.

**Manual management:**
```bash
# Check enforcement status
sudo aa-status | grep -E '(firecracker|fc-runner)'

# Switch to complain mode (log violations without blocking)
sudo aa-complain /etc/apparmor.d/usr.local.bin.firecracker

# Re-enforce
sudo aa-enforce /etc/apparmor.d/usr.local.bin.firecracker
```
