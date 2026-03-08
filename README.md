# fc-runner

A Rust orchestrator that polls GitHub for queued workflow jobs and boots an ephemeral [Firecracker](https://firecracker-microvm.github.io/) microVM per job. Each VM is fully isolated, boots in ~125 ms, runs the job, and is destroyed — no shared state between jobs.

## How It Works

```
GitHub Actions                fc-runner                    Firecracker
┌──────────┐    poll     ┌─────────────────┐   spawn    ┌──────────────┐
│  Queued   │◄──────────│   Orchestrator    │──────────►│   microVM     │
│   Jobs    │            │                   │           │  (ephemeral)  │
└──────────┘   JIT token │  Dedup (HashSet)  │  cleanup  │  Ubuntu 24.04 │
               ────────►│  tokio::spawn/job  │◄──────────│  actions-runner│
                         └─────────────────┘            └──────────────┘
```

1. **Poll** — queries the GitHub REST API for queued workflow runs matching configured labels
2. **Token** — requests a single-use JIT runner token for each new job (PAT or GitHub App)
3. **Prepare** — COW-copies the golden ext4 rootfs, injects secrets via MMDS or loop-mount
4. **Boot** — launches a Firecracker microVM with the prepared rootfs (~125 ms boot)
5. **Run** — the VM registers as an ephemeral GitHub runner, executes the job
6. **Cleanup** — VM exits, all artifacts (rootfs copy, config, logs) are deleted

## Features

- **Ephemeral VMs** — clean environment for every job, no cross-contamination
- **Fast boot** — Firecracker microVMs start in ~125 ms
- **Auto-provisioning** — kernel download, golden rootfs build (from Ubuntu cloud image via pure Rust qcow2 conversion), per-VM TAP networking, and NAT are all set up automatically at first startup
- **JIT tokens** — single-use, short-lived tokens (no static runner registration)
- **GitHub App auth** — authenticate as a GitHub App for higher rate limits and no PAT expiry management
- **MMDS secret injection** — inject secrets via Firecracker's built-in metadata service (no loop-mount needed)
- **Pool-based scaling** — named VM pools with per-pool repos, replica counts, and resource overrides
- **Prometheus metrics** — `/metrics` endpoint with job counts, VM boot duration, API rate limits, and more
- **Management API** — REST API (`/api/v1/status`, `/api/v1/vms`) for monitoring and VM management
- **Guest agent** — `fc-runner agent` runs inside VMs: reads MMDS, starts runner, reports state via VSOCK
- **CLI subcommands** — `server`, `agent`, `validate`, `ps`, `pools` (list/scale/pause/resume), `logs`
- **Org-level runners** — register runners at the GitHub organization level for cross-repo job pickup
- **Runtime pool management** — pause, resume, and scale pools at runtime via REST API or CLI
- **VSOCK guest agent** — host-guest communication via virtio-vsock with NDJSON protocol
- **Secret protection** — GitHub PAT stored via `secrecy::SecretString`, zeroized on drop, redacted in all logs
- **AppArmor** — ships restrictive profiles for both `firecracker` and `fc-runner` binaries
- **Concurrency control** — bounded by `max_concurrent_jobs` via `tokio::sync::Semaphore` (default: 4)
- **VM timeout** — configurable per-VM execution timeout (default: 3600s) kills hung jobs
- **Graceful shutdown** — SIGTERM/SIGINT handlers stop the poll loop and wait up to 5 min for active VMs
- **Rate-limit aware** — parses GitHub API `x-ratelimit-remaining` headers, warns and backs off automatically
- **Deduplication** — `HashSet<job_id>` prevents dispatching the same job twice
- **Structured logging** — `tracing` with configurable log levels via `RUST_LOG`

## Stack

| Layer | Technology |
|-------|-----------|
| Orchestrator | Rust + Tokio (async) |
| Hypervisor | Firecracker v1.14.2 |
| Guest OS | Ubuntu 24.04 Noble |
| Guest networking | virtio-net over per-VM TAP (pure Rust rtnetlink) + iptables NAT |
| CI platform | GitHub Actions REST API |
| Host init | systemd |
| Security | AppArmor, secrecy, Firecracker jailer (optional) |

## Prerequisites

- **Linux host** — Pop!_OS or Ubuntu 24.04 (bare-metal or nested virt enabled)
- **Rust toolchain** — install via [rustup](https://rustup.rs/)
- **GitHub PAT or App** — Fine-grained PAT (recommended: `Actions` + `Administration` permissions), Classic PAT (`repo` scope), or GitHub App. See [setup guide](docs/setup.md#github-token-setup) for step-by-step instructions.
- **KVM access** — one-time setup:

```bash
# Verify CPU virtualization (must return > 0)
grep -Eoc '(vmx|svm)' /proc/cpuinfo

# Detect CPU vendor and load the correct KVM module
# "vmx" in cpuinfo = Intel (use kvm_intel), "svm" = AMD (use kvm_amd)
lscpu | grep "Vendor ID"
sudo modprobe kvm_intel   # Intel (GenuineIntel)
sudo modprobe kvm_amd     # AMD (AuthenticAMD)

# Add your user to the kvm group (then log out/in, or use: newgrp kvm)
sudo usermod -aG kvm $USER
```

## Quick Start

### 1. Build

```bash
git clone https://github.com/silafood/fc-runner.git
cd fc-runner
cargo build --release
```

### 2. Install host dependencies

```bash
sudo bash install.sh
```

This installs system packages, Firecracker v1.14.2, AppArmor profiles, config templates, and the systemd service. Kernel, rootfs, and networking are handled automatically by fc-runner at startup.

### 3. Configure

```bash
sudo nano /etc/fc-runner/config.toml
```

Set at minimum:

```toml
[github]
token = "ghp_your_personal_access_token"
owner = "your-org"
repo = "your-repo"
# Or serve multiple repos:
# repos = ["repo-one", "repo-two"]

# Or use org-level runners:
# organization = "your-org"

# Or use GitHub App authentication instead of a PAT:
# [github.app]
# app_id = 12345
# installation_id = 67890
# private_key_path = "/etc/fc-runner/app-key.pem"
```

### 4. Start

```bash
sudo install -m 0755 target/release/fc-runner /usr/local/bin/fc-runner

# Option A: Run via systemd (production)
sudo systemctl start fc-runner
sudo journalctl -u fc-runner -f

# Option B: Run directly (development)
sudo fc-runner server --config /etc/fc-runner/config.toml
```

### 5. Use in your workflow

```yaml
# .github/workflows/build.yml
jobs:
  build:
    runs-on: [self-hosted, linux, firecracker]
    steps:
      - uses: actions/checkout@v4
      - run: echo "Running inside a Firecracker microVM!"
```

## CLI Usage

fc-runner uses clap-based subcommands:

```bash
# Start the server (orchestrator + management API)
fc-runner server --config /etc/fc-runner/config.toml

# Validate a config file without starting
fc-runner validate --config /etc/fc-runner/config.toml

# List running VMs (calls management API)
fc-runner ps --endpoint http://localhost:9090

# Pool management
fc-runner pools list --endpoint http://localhost:9090
fc-runner pools scale default --min-ready 3 --endpoint http://localhost:9090
fc-runner pools pause default --endpoint http://localhost:9090
fc-runner pools resume default --endpoint http://localhost:9090

# Stream VM logs
fc-runner logs --vm-id <id> --endpoint http://localhost:9090

# Guest agent (runs inside a Firecracker VM, not invoked manually)
fc-runner agent --log-level debug

# Print version
fc-runner --version
```

## Configuration

Full example at [`config.toml.example`](config.toml.example). Key sections:

| Section | Key fields |
|---------|-----------|
| `[github]` | `token`, `owner`, `repo`/`repos`, `organization`, `labels`; or `[github.app]` for App auth |
| `[firecracker]` | `kernel_path`, `rootfs_golden`, `vcpu_count`, `mem_size_mib`, `secret_injection`, `vsock_enabled` |
| `[runner]` | `work_dir`, `poll_interval_secs`, `max_concurrent_jobs`, `vm_timeout_secs`, `warm_pool_size` |
| `[[pool]]` | Named pools: `name`, `repos`, `min_ready`, `max_ready`, per-pool `vcpu_count`/`mem_size_mib` |
| `[network]` | `host_ip`, `guest_ip`, `cidr`, `dns`, `allowed_networks` |
| `[server]` | `enabled`, `listen_addr`, `api_key` — Prometheus metrics + management API |

See [docs/configuration.md](docs/configuration.md) for the full reference.

## Project Structure

```
fc-runner/
├── src/
│   ├── main.rs           # Entry point with clap CLI dispatch
│   ├── cli.rs            # CLI subcommand definitions (server, agent, ps, pools, etc.)
│   ├── api_client.rs     # HTTP client for CLI→server management API calls
│   ├── agent.rs          # Guest agent: MMDS reader, runner launcher, VSOCK reporter
│   ├── config.rs         # Typed TOML config with validation
│   ├── github.rs         # GitHub API client (PAT + App auth, repo + org level)
│   ├── firecracker.rs    # MicroVm lifecycle: prepare → run → cleanup (MMDS + mount modes)
│   ├── netlink.rs        # Pure-Rust TAP device management (rtnetlink + nix ioctl)
│   ├── orchestrator.rs   # Poll/dispatch loop with dedup (JIT, warm pool, named pools)
│   ├── setup.rs          # KVM checks, kernel/rootfs provisioning, network, AppArmor
│   ├── metrics.rs        # Prometheus metrics registry and counters
│   ├── server.rs         # HTTP server: /metrics, /healthz, management + pool API
│   ├── pool.rs           # Named VM pool manager with runtime pause/resume/scale
│   └── vsock.rs          # Host-side VSOCK listener for guest agent communication
├── guest_configs/
│   ├── fetch-mmds-env.sh            # Guest-side MMDS metadata fetch script
│   └── microvm-kernel-ci-*.config   # Firecracker kernel configs (x86_64 + aarch64)
├── .github/workflows/
│   └── release.yml       # CI: build binary + kernel + rootfs, publish release
├── apparmor/
│   ├── usr.local.bin.firecracker   # Restrictive profile for Firecracker VMM
│   └── usr.local.bin.fc-runner     # Restrictive profile for orchestrator
├── docs/
│   ├── architecture.md   # System design and module overview
│   ├── setup.md          # Installation guide
│   ├── configuration.md  # Config reference
│   └── troubleshooting.md
├── Cargo.toml
├── config.toml.example
├── fc-runner.service      # systemd unit
├── install.sh             # Host setup script
└── build-v611-linux.sh    # Manual golden rootfs + kernel provisioning
```

## Security

**Defense in depth:**

| Layer | Mechanism |
|-------|-----------|
| VM isolation | Firecracker microVM (KVM-based, minimal attack surface) |
| Secret handling | `secrecy::SecretString` — zeroized on drop, redacted in Debug/logs |
| Token scope | JIT tokens are single-use, short-lived, per-job |
| Token injection | MMDS metadata service (default) or written to ext4 image file — never kernel cmdline or `/proc`-visible env vars |
| Path validation | Symlink checks on all critical paths at config load time |
| Mount safety | TOCTOU protection via `mountpoint -q` verification; umount retry with lazy fallback |
| Filesystem | AppArmor profiles restrict both binaries to minimum required paths |
| Network | AppArmor `net_admin` capability scoped; Firecracker has no network access in its profile |
| Rate limiting | Parses `x-ratelimit-remaining` headers; warns at < 100, backs off at < 10 |
| Process | Firecracker `jailer` for chroot + seccomp-BPF + UID/GID drop (enable via `jailer_path` config) |
| Host hardening | systemd: `NoNewPrivileges`, `ProtectSystem=strict`, `MemoryDenyWriteExecute`, restricted capabilities |
| Cleanup | All VM artifacts deleted after every job, even on failure |

**AppArmor profiles:**

- `usr.local.bin.firecracker` — read-only kernel, r/w only per-VM files, KVM + TAP access, deny-all default
- `usr.local.bin.fc-runner` — read-only config, r/w work dir, mount capability, can only spawn firecracker/jailer

## Networking

```
Host (172.16.<slot>.1/24) ◄──► tap-fc<slot> ◄──► Guest eth0 (172.16.<slot>.2/24)
         │
    iptables MASQUERADE + TCP MSS clamping
         │
      Internet / GitHub API
```

Each VM gets its own TAP device with a unique subnet. TAP creation uses pure Rust `ioctl(TUNSETIFF)` via the `nix` crate, and IP/link management uses the `rtnetlink` crate. IP forwarding and NAT rules are configured automatically at startup.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `KVM not available` | `sudo modprobe kvm_intel` (or `kvm_amd`) |
| Permission denied on `/dev/kvm` | `sudo usermod -aG kvm $USER && newgrp kvm` |
| VM boots but runner never registers | Check JIT token, verify TAP NAT rules |
| `mount: /dev/loop*: failed` | `sudo modprobe loop max_loop=64` |
| GitHub API 422 on JIT config | Check `runner_group_id` and PAT `repo` scope |
| Rootfs runs out of space | Delete golden rootfs and restart to rebuild |
| AppArmor `DENIED` | Check `dmesg \| grep DENIED`, update profile, `apparmor_parser -r` |
| Guest VM emergency mode | Delete golden rootfs and restart (fstab/EFI mount fix) |

See [docs/troubleshooting.md](docs/troubleshooting.md) for detailed diagnostics.

## Common Commands

```bash
# Validate config without starting
fc-runner validate --config /etc/fc-runner/config.toml

# Check service status
sudo systemctl status fc-runner

# Live logs
sudo journalctl -u fc-runner -f

# Rebuild and redeploy
cargo build --release
sudo install -m 0755 target/release/fc-runner /usr/local/bin/fc-runner
sudo systemctl restart fc-runner

# List running VMs via CLI
fc-runner ps --endpoint http://localhost:9090

# Pool management via CLI
fc-runner pools list --endpoint http://localhost:9090
fc-runner pools scale default --min-ready 3 --endpoint http://localhost:9090
fc-runner pools pause default --endpoint http://localhost:9090
fc-runner pools resume default --endpoint http://localhost:9090

# Prometheus metrics (default port 9090)
curl -s http://localhost:9090/metrics

# Management API — server status
curl -s http://localhost:9090/api/v1/status | jq .

# Management API — list active VMs
curl -s http://localhost:9090/api/v1/vms | jq .

# Management API — list pools
curl -s http://localhost:9090/api/v1/pools | jq .

# Health check
curl -s http://localhost:9090/healthz

# Check AppArmor enforcement
sudo aa-status | grep -E '(firecracker|fc-runner)'

# Force rebuild golden rootfs
sudo rm /opt/fc-runner/runner-rootfs-golden.ext4
sudo systemctl restart fc-runner
```

## Documentation

- [Architecture](docs/architecture.md) — system design, module overview, security model
- [Setup Guide](docs/setup.md) — installation and verification
- [Configuration Reference](docs/configuration.md) — all config options
- [Troubleshooting](docs/troubleshooting.md) — common issues and diagnostics

## License

MIT
