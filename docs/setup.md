# Setup Guide

## Prerequisites

- **Linux host** running Pop!_OS or Ubuntu 24.04 (bare-metal or nested virt enabled)
- **Rust toolchain** — install via [rustup](https://rustup.rs/)
- **GitHub PAT** with `repo` scope

### KVM Setup (one-time, requires sudo)

fc-runner checks KVM access automatically at startup and will provide clear error messages if something is missing. Here's how to set it up:

```bash
# 1. Verify CPU supports virtualization (must return > 0)
grep -Eoc '(vmx|svm)' /proc/cpuinfo

# 2. Load KVM module if not already loaded
sudo modprobe kvm_intel   # Intel CPUs
# or
sudo modprobe kvm_amd     # AMD CPUs

# 3. Verify /dev/kvm exists
ls -la /dev/kvm
# Should show: crw-rw---- 1 root kvm ...

# 4. Add your user to the kvm group (avoids needing root for KVM access)
sudo usermod -aG kvm $USER
newgrp kvm
```

After this, fc-runner handles everything else automatically at startup:
- Downloads the guest kernel if missing
- Builds the golden rootfs if missing
- Creates TAP device and configures NAT rules

## Quick Start

### 1. Clone and build

```bash
git clone <repo-url> fc-runner
cd fc-runner
cargo build --release
```

### 2. Run the install script

The install script installs system dependencies, Firecracker binaries, config files, and the systemd service.

```bash
sudo bash install.sh
```

This will:
- Install system dependencies (debootstrap, curl, jq, iptables, etc.)
- Install Firecracker v1.14.2 and jailer to `/usr/local/bin/`
- Create directories and copy config templates to `/etc/fc-runner/`
- Install and enable the `fc-runner.service` systemd unit

Kernel download, rootfs building, and network setup are handled automatically by fc-runner at startup.

### 3. Configure

Edit `/etc/fc-runner/config.toml` with your GitHub PAT and repository:

```bash
sudo nano /etc/fc-runner/config.toml
```

At minimum, set:
- `github.token` — your PAT
- `github.owner` — repository owner
- `github.repo` — repository name

For production hardening, enable the jailer:
```toml
[firecracker]
jailer_path = "/usr/local/bin/jailer"
jailer_uid = 1000
jailer_gid = 1000
```
This runs each VM inside a chroot with seccomp-BPF filtering and dropped privileges. `jailer_uid` and `jailer_gid` are required when `jailer_path` is set.

### 4. Install the binary and start

```bash
sudo install -m 0755 target/release/fc-runner /usr/local/bin/fc-runner
sudo systemctl start fc-runner
sudo journalctl -u fc-runner -f
```

### 5. Trigger a workflow

Push a commit or manually trigger a workflow in your repo that uses:

```yaml
runs-on: [self-hosted, linux, firecracker]
```

fc-runner will pick up the job, boot a VM, and run it.

## Verify the Installation

```bash
# Check service status
sudo systemctl status fc-runner

# Check Firecracker is installed
firecracker --version

# Check KVM is available
ls /dev/kvm

# Check TAP interface
ip addr show tap-fc0

# Check golden rootfs exists
ls -lh /opt/fc-runner/runner-rootfs-golden.ext4

# Check COW reflink support (btrfs/xfs only; ext4 falls back to full copy)
df -Th /var/lib/fc-runner/vms
```
