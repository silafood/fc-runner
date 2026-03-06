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
5. Spawns a `tokio::spawn` task per new job

The job ID is removed from the seen set after the task completes, allowing retry if the VM failed before the runner registered.

## Concurrency

Each job runs in its own tokio task. There is currently no concurrency limit — for production use, add a `tokio::sync::Semaphore` in the orchestrator.

## Security

- JIT tokens are written into the ext4 image (not kernel cmdline) and deleted on VM teardown
- `--no-api` disables the Firecracker management socket
- The GitHub PAT is never logged (`secrecy::SecretString` with custom Debug impl redacts it, zeroized on drop)
- The `jailer` binary is installed but not wired in by default — enable it for production hardening

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
