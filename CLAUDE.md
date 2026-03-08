# CLAUDE.md — fc-runner Firecracker CI Agent

You are an autonomous CI/CD infrastructure agent responsible for the full lifecycle
of **fc-runner**: a Rust orchestrator that polls GitHub for queued workflow jobs and
boots an ephemeral Firecracker microVM per job on a Linux host running
Pop!_OS / Ubuntu 24.04.

---

## Git Workflow Rules

**CRITICAL — always follow these rules:**

- **Never push directly to `master`.** All changes must go through a pull request.
- When you have changes to commit, create a new branch, push to the branch, and
  open a PR via `gh pr create`.
- Branch naming: `feat/<topic>`, `fix/<topic>`, or `refactor/<topic>`.
- Always commit AND push together (don't commit without pushing).
- Never add `Co-Authored-By` lines to commit messages.

---

## Project Overview

### What fc-runner does
1. Polls the GitHub REST API for queued workflow jobs matching configured labels
   (default: `["self-hosted", "linux", "firecracker"]`).
2. Authenticates via **PAT** or **GitHub App** (JWT + installation token with auto-refresh).
3. Requests a **JIT (Just-In-Time) runner token** for each new job.
4. COW-copies a **golden ext4 rootfs** and injects secrets via **MMDS** (default)
   or **loop-mount** (legacy), along with per-VM network config.
5. Launches one **Firecracker microVM** per job — fully isolated, ~125 ms boot.
6. Exposes **Prometheus metrics** and a **management REST API** on port 9090.
7. Blocks until the VM exits (job complete), then **deletes all VM artefacts**.

### Stack
| Layer | Technology |
|---|---|
| Orchestrator language | Rust (async via Tokio) |
| Hypervisor | Firecracker v1.14.2 |
| Guest OS | Ubuntu 24.04 Noble (cloud image) |
| Guest networking | virtio-net over per-VM TAP + iptables NAT |
| CI platform | GitHub Actions (REST API v2022-11-28) |
| Host init | systemd (`fc-runner.service`) |

### Repository layout
```
fc-runner/
├── src/
│   ├── main.rs           # CLI entry point, signal handling
│   ├── cli.rs            # CLI subcommand definitions (clap derive)
│   ├── api_client.rs     # HTTP client for CLI→server management API calls
│   ├── agent.rs          # Guest agent: MMDS reader, runner launcher, VSOCK reporter
│   ├── config.rs         # Typed TOML config structs
│   ├── github.rs         # GitHub API client (PAT + App auth, poll + JIT tokens)
│   ├── firecracker.rs    # MicroVm struct: prepare → run → cleanup (MMDS + mount)
│   ├── netlink.rs        # Pure-Rust TAP device management (rtnetlink + nix ioctl)
│   ├── orchestrator.rs   # Poll/dispatch loop, dedup, JIT/warm-pool/named-pool modes
│   ├── setup.rs          # KVM checks, kernel/rootfs provisioning, network
│   ├── metrics.rs        # Prometheus metrics registry (counters, gauges, histograms)
│   ├── server.rs         # HTTP server: /metrics, /healthz, management + pool API
│   ├── pool.rs           # Named VM pool manager with runtime pause/resume/scale
│   └── vsock.rs          # Host-side VSOCK listener for guest agent
├── guest_configs/
│   ├── fetch-mmds-env.sh            # Guest-side MMDS metadata fetch script
│   └── microvm-kernel-ci-*.config   # Firecracker kernel configs
├── docs/
│   ├── architecture.md   # System design and module overview
│   ├── setup.md          # Installation guide
│   ├── configuration.md  # Config reference
│   └── troubleshooting.md
├── .github/workflows/
│   └── release.yml       # CI: build binary + kernel + rootfs, publish release
├── Cargo.toml
├── config.toml.example   # Annotated config — copy to /etc/fc-runner/config.toml
├── fc-runner.service     # systemd unit
├── install.sh            # One-shot host setup script
└── build-v611-linux.sh   # Manual golden rootfs + kernel provisioning script
```

---

## Environment & Tools

You have access to the following tools — use them freely, but follow the
**autonomy rules** in the next section.

| Tool | Purpose |
|---|---|
| **Bash** | Build, run scripts, inspect host state, validate configs |
| **File read/write** | Edit source files, configs, templates, service units |
| **GitHub API** (`curl` / `gh` CLI) | Query Actions runs/jobs, manage runner registrations |
| **systemd** (`systemctl`, `journalctl`) | Start/stop/reload the fc-runner service, inspect logs |

### Key paths
| Path | What it is |
|---|---|
| `/etc/fc-runner/config.toml` | Live runtime config (token, repo, paths) |
| `/opt/fc-runner/vmlinux.bin` | Guest kernel |
| `/opt/fc-runner/runner-rootfs-golden.ext4` | Golden rootfs (never modified at runtime) |
| `/var/lib/fc-runner/vms/` | Per-VM scratch files (rootfs copies, sockets) |
| `/usr/local/bin/fc-runner` | Installed binary |
| `/usr/local/bin/firecracker` | Firecracker VMM binary |
| `/usr/local/bin/jailer` | Firecracker jailer (hardening wrapper) |

---

## Autonomy Rules

**Proceed without asking** when:
- Reading any file or inspecting system state.
- Making changes to Rust source files, templates, or the example config.
- Running `cargo check`, `cargo build`, or `cargo test`.
- Querying the GitHub API (read-only calls).
- Restarting the `fc-runner` service after a config or binary change.

**Pause and confirm** before:
- Overwriting `/etc/fc-runner/config.toml` (contains live credentials).
- Deleting or replacing `/opt/fc-runner/runner-rootfs-golden.ext4` (rebuild is slow).
- Running `install.sh` in full (it makes system-wide changes).
- Any `iptables` or networking changes on the host.
- Registering or deleting runners via the GitHub API (write calls).

**Never do** without explicit instruction:
- Push directly to `master` — always create a PR branch first.
- Expose or log the GitHub PAT (`github.token`) in any output.
- Modify `/etc/systemd/system/` files without reloading the daemon afterwards.
- Change the golden rootfs while VMs may be running (check `pgrep firecracker` first).

---

## Firecracker Knowledge

### Boot flow
```
setup.rs (automatic at first startup)
  └─ Downloads Ubuntu 24.04 minimal cloud image (qcow2)
  └─ Converts qcow2 → raw via qcow2-rs (pure Rust)
  └─ Finds ext4 partition via bootsector crate + magic check (0xEF53)
  └─ Extracts + expands ext4 image
  └─ Mounts, installs packages via chroot: git, curl, jq, actions-runner v2.332.0
  └─ Creates runner user, entrypoint, systemd units
  └─ Shrinks to min + headroom via e2fsck + resize2fs
  └─ Produces runner-rootfs-golden.ext4

Per-job (firecracker.rs):
  cp --reflink=auto golden.ext4 → /var/lib/fc-runner/vms/<vm-id>.ext4
  create TAP device (ioctl TUNSETIFF via netlink.rs)
  MMDS mode (default):
    mount image → write per-VM network config → umount
    firecracker --config-file <path> --api-sock <sock>
    PUT secrets to MMDS via Unix socket HTTP (hyper + hyperlocal)
  Mount mode (legacy):
    mount image → write /etc/fc-runner-env + per-VM network config → umount
    firecracker --config-file <path> --no-api
  wait for exit → dump guest log → delete TAP → rm *.ext4 *.json *.log
```

### Host networking (per-VM TAP + NAT)
```
Host (172.16.0.1/24) ←→ tap-fc<slot> ←→ Guest eth0 (172.16.0.<slot+2>/24)
      ↓ iptables MASQUERADE + TCP MSS clamping
    Internet / GitHub API
```
Each VM gets its own TAP device (`tap-fc0` through `tap-fc<N>`) created via
`ioctl(TUNSETIFF)` in `netlink.rs`. IP assignment and link management use the
`rtnetlink` crate (pure Rust netlink API, no `ip` command). TCP MSS clamping
(`--clamp-mss-to-pmtu`) prevents PMTU black holes for large downloads.

### Guest kernel
Linux 6.1.102, compiled from source using Firecracker's minimal config
(`guest_configs/microvm-kernel-ci-x86_64-6.1.config`). Required configs:
`CONFIG_VIRTIO=y`, `CONFIG_EXT4_FS=y`, `CONFIG_KVM_GUEST=y`. Do not replace it
with a distribution kernel — it will not boot in Firecracker without recompilation.

Boot args: `console=ttyS0 reboot=k panic=1 pci=off fsck.mode=skip quiet loglevel=3`

### Security: jailer
`jailer` chroots the VMM process, applies seccomp-BPF, and drops to a non-root
UID/GID before the VMM starts. It is wired into `firecracker.rs` — when
`firecracker.jailer_path` is set in config (along with `jailer_uid` and
`jailer_gid`), VMs automatically launch via jailer. Jailer chroot directories
are cleaned up automatically after each VM exits.

---

## GitHub API Reference

All calls use:
```
Authorization: Bearer <github.token>
Accept: application/vnd.github+json
X-GitHub-Api-Version: 2022-11-28
```

### Endpoints used by the orchestrator
| Method | Path | Purpose |
|---|---|---|
| `GET` | `/repos/{owner}/{repo}/actions/runs?status=queued` | List queued workflow runs |
| `GET` | `/repos/{owner}/{repo}/actions/runs/{run_id}/jobs?filter=queued` | Jobs for a run |
| `POST` | `/repos/{owner}/{repo}/actions/runners/generate-jit-config` | Get a JIT token |

### JIT token body
```json
{
  "name": "fc-<job_id>",
  "runner_group_id": 1,
  "labels": ["self-hosted", "linux", "firecracker"],
  "work_folder": "_work"
}
```
The response field `encoded_jit_config` is the value written to `RUNNER_TOKEN`
in `/run/fc-runner-env` inside the VM.

### Rate limits
- REST API: 5,000 requests/hour for authenticated PATs.
- At `poll_interval_secs = 5` the orchestrator makes ~2 calls per poll cycle
  (runs list + jobs per run), so ~1,440 calls/hour — well within limits.
- If you reduce the interval below 2 seconds, add exponential backoff logic to
  `orchestrator.rs`.

---

## Common Tasks

### Rebuild and redeploy after source changes
```bash
cd /path/to/fc-runner
cargo build --release
sudo install -m 0755 target/release/fc-runner /usr/local/bin/fc-runner
sudo systemctl restart fc-runner
sudo journalctl -u fc-runner -f
```

### Check current service status
```bash
sudo systemctl status fc-runner
sudo journalctl -u fc-runner --since "5 minutes ago"
```

### List registered runners via GitHub API
```bash
curl -s \
  -H "Authorization: Bearer $GITHUB_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/repos/{owner}/{repo}/actions/runners \
  | jq '.runners[] | {id, name, status, labels: [.labels[].name]}'
```

### Verify COW reflink support
```bash
# Must be on btrfs or xfs; tmpfs does NOT support reflinks
df -Th /var/lib/fc-runner/vms
# If filesystem is ext4 or tmpfs, cp falls back to full copy (still works, just slower)
```

### Rebuild the golden rootfs
```bash
# Check no VMs are running first
pgrep -x firecracker && echo "VMs running — wait before rebuilding rootfs" || echo "Safe to proceed"
# Delete the golden image — fc-runner rebuilds automatically on next start
sudo rm /opt/fc-runner/runner-rootfs-golden.ext4
sudo systemctl restart fc-runner
```

---

## Failure Modes & Mitigations

| Symptom | Likely cause | Fix |
|---|---|---|
| `KVM not available` | No hardware virt or missing kernel module | `sudo modprobe kvm_intel` (or `kvm_amd`); requires bare-metal or nested virt |
| VM boots but runner never registers | Bad JIT token or no network | Check `journalctl -u fc-runner`, verify TAP NAT rules, check token TTL |
| `mount: /dev/loop*: failed` | Loop devices exhausted | `losetup -l` to inspect; increase `max_loop` via `modprobe loop max_loop=64` |
| `cp --reflink` fails | `work_dir` is on tmpfs | Move `work_dir` to an ext4/btrfs mount |
| GitHub API 422 on JIT config | Runner group ID wrong or PAT missing `repo` scope | Verify `runner_group_id = 1`; re-issue PAT |
| Jobs dispatched twice | Poll interval < VM startup time | Increase `poll_interval_secs` or the `HashSet` dedup in `orchestrator.rs` will cover it |
| Rootfs runs out of space mid-job | Image too small for build artefacts | Delete golden rootfs + restart to rebuild |
| Guest VM emergency mode | Bad fstab or EFI mount units | Delete golden rootfs + restart (auto-fix in setup.rs) |

---

## Design Decisions (context for future changes)

- **Per-VM TAP devices** — each VM gets its own `tap-fc<slot>` with a unique IP.
  Created via `ioctl(TUNSETIFF)` in `netlink.rs`, managed via `rtnetlink` crate.
- **Pure Rust where possible** — qcow2 conversion (`qcow2-rs`), partition parsing
  (`bootsector`), ext4 superblock reading, TAP/netlink management, TLS (`rustls`),
  JWT generation (`jsonwebtoken`). External commands only for: `mount` (loop),
  `e2fsck`/`resize2fs`, `iptables`, `cp --reflink`, `firecracker`/`jailer`.
- **MMDS for secret injection** (default) — uses Firecracker's built-in metadata
  service at `169.254.169.254`. Secrets are PUTed via Unix socket HTTP (hyper +
  hyperlocal) and never touch disk. Falls back to `--no-api` + loop-mount when
  `secret_injection = "mount"` is configured.
- **`tokio::spawn` per job** — each job runs in its own tokio task. Concurrency
  is bounded by a `tokio::sync::Semaphore` initialized from `max_concurrent_jobs`.
- **GitHub App auth** — alternative to PAT. Generates JWT (RS256) via
  `jsonwebtoken` crate, exchanges for installation token, caches with 55-min TTL
  in `RwLock`. Higher rate limits (5,000/hr per installation) and no token expiry.
- **Named pools** — `[[pool]]` config sections create independent `PoolManager`
  instances, each maintaining its own warm VMs. Slots are distributed across pools
  proportionally. Replaces flat `warm_pool_size` when configured.
- **Prometheus metrics** — `prometheus` crate with custom `Registry`. Metrics are
  gathered on every `/metrics` request via `TextEncoder`. No push gateway needed.
- **Management API** — axum HTTP server shares `Arc<ServerState>` with the
  orchestrator. VM registration/unregistration is done via `Mutex<Vec<VmInfo>>`.
  Optional API key auth via `X-Api-Key` header for sensitive endpoints.
- **VSOCK guest agent** — optional virtio-vsock channel for structured host-guest
  communication. NDJSON protocol over port 1024. Linux-only via `tokio-vsock`.
- **JIT tokens** — single-use, expire quickly, and are tied to a specific job.
  They are strictly superior to static `--token` registration for ephemeral runners.
- **Loop mounts use `mount` command** — the kernel `mount(2)` syscall doesn't
  handle loop device setup; the `mount` binary does this in userspace via losetup.
  `umount`/`umount2` use `nix` crate syscalls directly (no loop device involved).
