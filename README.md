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
2. **Token** — requests a single-use JIT runner token for each new job
3. **Prepare** — COW-copies the golden ext4 rootfs, mounts it, injects the JIT token
4. **Boot** — launches a Firecracker microVM with the prepared rootfs (~125 ms boot)
5. **Run** — the VM registers as an ephemeral GitHub runner, executes the job
6. **Cleanup** — VM exits, all artifacts (rootfs copy, config, logs) are deleted

## Features

- **Ephemeral VMs** — clean environment for every job, no cross-contamination
- **Fast boot** — Firecracker microVMs start in ~125 ms
- **Auto-provisioning** — kernel, golden rootfs, TAP networking, and NAT are set up automatically at first startup
- **JIT tokens** — single-use, short-lived tokens (no static runner registration)
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
| Guest networking | virtio-net over TAP + iptables NAT |
| CI platform | GitHub Actions REST API |
| Host init | systemd |
| Security | AppArmor, secrecy, Firecracker jailer (optional) |

## Prerequisites

- **Linux host** — Pop!_OS or Ubuntu 24.04 (bare-metal or nested virt enabled)
- **Rust toolchain** — install via [rustup](https://rustup.rs/)
- **GitHub PAT** — with `repo` scope
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
```

### 4. Start

```bash
sudo install -m 0755 target/release/fc-runner /usr/local/bin/fc-runner
sudo systemctl start fc-runner
sudo journalctl -u fc-runner -f
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

## Configuration

Full example at [`config.toml.example`](config.toml.example). Key sections:

| Section | Key fields |
|---------|-----------|
| `[github]` | `token`, `owner`, `repo`, `runner_group_id` (default: 1), `labels` |
| `[firecracker]` | `kernel_path`, `rootfs_golden`, `vcpu_count` (default: 2), `mem_size_mib` (default: 2048) |
| `[runner]` | `work_dir`, `poll_interval_secs` (default: 5), `max_concurrent_jobs` (default: 4), `vm_timeout_secs` (default: 3600) |
| `[network]` | `tap_device` (default: tap-fc0), `host_ip`, `guest_ip`, `cidr`, `dns` (default: 8.8.8.8, 1.1.1.1) |

See [docs/configuration.md](docs/configuration.md) for the full reference.

## Project Structure

```
fc-runner/
├── src/
│   ├── main.rs           # Entry point, signal handling, config loading
│   ├── config.rs         # Typed TOML config with validation
│   ├── github.rs         # GitHub API client (poll + JIT tokens)
│   ├── firecracker.rs    # MicroVm lifecycle: prepare → run → cleanup
│   ├── orchestrator.rs   # Poll/dispatch loop with dedup
│   └── setup.rs          # KVM checks, kernel/rootfs provisioning, network, AppArmor
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
├── vm-config.json.template
├── fc-runner.service      # systemd unit
└── install.sh             # Host setup script
```

## Security

**Defense in depth:**

| Layer | Mechanism |
|-------|-----------|
| VM isolation | Firecracker microVM (KVM-based, minimal attack surface) |
| Secret handling | `secrecy::SecretString` — zeroized on drop, redacted in Debug/logs |
| Token scope | JIT tokens are single-use, short-lived, per-job |
| Token injection | Written to ext4 image file (not kernel cmdline or env vars visible in `/proc`), `chmod 0600` |
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
Host (172.16.0.1/24) ◄──► tap-fc0 ◄──► Guest eth0 (172.16.0.2/24)
         │
    iptables MASQUERADE
         │
      Internet / GitHub API
```

TAP device, IP forwarding, and NAT rules are configured automatically at startup. All values are configurable via the `[network]` config section.

For parallel VMs at scale, provision a TAP per VM with unique guest MAC/IP pairs.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `KVM not available` | `sudo modprobe kvm_intel` (or `kvm_amd`) |
| Permission denied on `/dev/kvm` | `sudo usermod -aG kvm $USER && newgrp kvm` |
| VM boots but runner never registers | Check JIT token, verify TAP NAT rules |
| `mount: /dev/loop*: failed` | `sudo modprobe loop max_loop=64` |
| GitHub API 422 on JIT config | Check `runner_group_id` and PAT `repo` scope |
| Rootfs runs out of space | Increase `ROOTFS_SIZE_MIB` in `setup.rs` and rebuild |

See [docs/troubleshooting.md](docs/troubleshooting.md) for detailed diagnostics.

## Common Commands

```bash
# Check service status
sudo systemctl status fc-runner

# Live logs
sudo journalctl -u fc-runner -f

# Rebuild and redeploy
cargo build --release
sudo install -m 0755 target/release/fc-runner /usr/local/bin/fc-runner
sudo systemctl restart fc-runner

# Check for running VMs
pgrep -a firecracker

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
