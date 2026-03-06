# CLAUDE.md — fc-runner Firecracker CI Agent

You are an autonomous CI/CD infrastructure agent responsible for the full lifecycle
of **fc-runner**: a Rust orchestrator that polls GitHub for queued workflow jobs and
boots an ephemeral Firecracker microVM per job on a Linux host running
Pop!_OS / Ubuntu 24.04.

---

## Project Overview

### What fc-runner does
1. Polls the GitHub REST API for queued workflow jobs matching configured labels
   (default: `["self-hosted", "linux", "firecracker"]`).
2. Requests a **JIT (Just-In-Time) runner token** for each new job.
3. COW-copies a **golden ext4 rootfs** and injects the JIT token + repo URL into
   `/run/fc-runner-env` inside the image.
4. Launches one **Firecracker microVM** per job — fully isolated, ~125 ms boot.
5. Blocks until the VM exits (job complete), then **deletes all VM artefacts**.

### Stack
| Layer | Technology |
|---|---|
| Orchestrator language | Rust (async via Tokio) |
| Hypervisor | Firecracker v1.14.2 |
| Guest OS | Ubuntu 24.04 Noble (debootstrapped) |
| Guest networking | virtio-net over TAP + iptables NAT |
| CI platform | GitHub Actions (REST API v2022-11-28) |
| Host init | systemd (`fc-runner.service`) |

### Repository layout
```
fc-runner/
├── src/
│   ├── main.rs           # CLI entry point, signal handling
│   ├── config.rs         # Typed TOML config structs
│   ├── github.rs         # GitHub API client (poll + JIT tokens)
│   ├── firecracker.rs    # MicroVm struct: prepare → run → cleanup
│   └── orchestrator.rs   # Poll/dispatch loop, dedup via HashSet<job_id>
├── Cargo.toml
├── config.toml.example   # Annotated config — copy to /etc/fc-runner/config.toml
├── vm-config.json.template  # Firecracker VM JSON with __PLACEHOLDERS__
├── fc-runner.service     # systemd unit
└── install.sh            # One-shot host setup script
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
| `/etc/fc-runner/vm-config.json.template` | Firecracker VM JSON template |
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
- Commit or push to any Git repository.
- Expose or log the GitHub PAT (`github.token`) in any output.
- Modify `/etc/systemd/system/` files without reloading the daemon afterwards.
- Change the golden rootfs while VMs may be running (check `pgrep firecracker` first).

---

## Firecracker Knowledge

### Boot flow
```
install.sh
  └─ debootstrap noble → /tmp/fc-rootfs
       └─ installs: git, curl, jq, actions-runner v2.323.0
       └─ writes /entrypoint.sh (registers runner → runs job → reboots)
  └─ packages rootfs → runner-rootfs-golden.ext4  (4 GiB ext4)

Per-job (orchestrator.rs):
  cp --reflink=auto golden.ext4 → /var/lib/fc-runner/vms/<vm-id>.ext4
  mount image → write /run/fc-runner-env → umount
  firecracker --config-file <rendered-json> --no-api
  wait for exit → rm *.ext4 *.sock *.json
```

### vm-config.json.template placeholders
| Placeholder | Replaced with |
|---|---|
| `__KERNEL_PATH__` | `firecracker.kernel_path` from config |
| `__ROOTFS_PATH__` | Per-VM COW copy path |
| `__VCPU_COUNT__` | `firecracker.vcpu_count` |
| `__MEM_MIB__` | `firecracker.mem_size_mib` |
| `__TAP_IFACE__` | `firecracker.tap_interface` |
| `__LOG_PATH__` | `/var/lib/fc-runner/vms/<vm-id>.log` |
| `__VM_ID__` | UUID-based VM identifier |

### Host networking (TAP + NAT)
```
Host (172.16.0.1/24) ←→ tap-fc0 ←→ Guest eth0 (172.16.0.2/24)
      ↓ iptables MASQUERADE
    Internet / GitHub API
```
Each VM shares the same TAP interface and guest MAC. This works because only one
VM uses the interface at a time in the default single-TAP setup. For true parallel
VMs at scale, provision a TAP per VM and a unique guest MAC/IP per VM.

### Guest kernel requirements
The AWS-provided quickstart kernel (`vmlinux.bin`) covers all needed features:
`CONFIG_VIRTIO=y`, `CONFIG_EXT4_FS=y`, `CONFIG_KVM_GUEST=y`. Do not replace it
with a distribution kernel — it will not boot in Firecracker without recompilation.

### Security: jailer
`jailer` chroots the VMM process, applies seccomp-BPF, and drops to a non-root
UID/GID before the VMM starts. It is installed but not wired into the orchestrator
by default. To enable it, replace the `Command::new(&self.cfg.binary_path)` call
in `firecracker.rs` with a `jailer` invocation following the pattern in the
README/document context.

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

### Smoke-test Firecracker without a real job
```bash
# Boot VM directly (will fail at runner registration without a valid token,
# but verifies the kernel + rootfs + networking work)
sudo firecracker \
  --api-sock /tmp/test.sock \
  --config-file /etc/fc-runner/vm-config.json.template \
  --no-api
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
# Then re-run just the rootfs section of install.sh, or delete the golden image
# to trigger a full rebuild on the next install.sh run:
sudo rm /opt/fc-runner/runner-rootfs-golden.ext4
sudo bash install.sh   # confirm when prompted
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
| Rootfs runs out of space mid-job | 4 GiB image too small for build artefacts | Increase `count=4096` in `dd` command and re-bootstrap |

---

## Design Decisions (context for future changes)

- **One TAP interface for all VMs** — acceptable for low concurrency. The MAC
  collision is harmless when VMs are time-sliced. For high concurrency, create a
  TAP per VM in `firecracker.rs::write_vm_config` and allocate unique IPs/MACs.
- **`--no-api` flag** — disables the Firecracker management API socket after
  boot. This reduces the attack surface and simplifies cleanup. Remove it only if
  you need to pause/snapshot VMs mid-job.
- **`tokio::spawn` per job** — the orchestrator spawns an unbounded number of
  concurrent tasks. Add a `tokio::sync::Semaphore` in `orchestrator.rs` if you
  want to cap parallelism (e.g., limited by host RAM).
- **Secret injection via mounted image** — credentials are written to ext4, not
  passed via kernel cmdline (which would appear in `/proc/cmdline` inside the VM).
  The file is deleted on VM teardown.
- **JIT tokens** — single-use, expire quickly, and are tied to a specific job.
  They are strictly superior to static `--token` registration for ephemeral runners.
