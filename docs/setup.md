# Setup Guide

## Prerequisites

- **Linux host** running Pop!_OS or Ubuntu 24.04 (bare-metal or nested virt enabled)
- **KVM support** — verify with `lsmod | grep kvm`
- **Root access** — Firecracker and loop mounts require root
- **Rust toolchain** — install via [rustup](https://rustup.rs/)
- **GitHub PAT** with `repo` scope

## Quick Start

### 1. Clone and build

```bash
git clone <repo-url> fc-runner
cd fc-runner
cargo build --release
```

### 2. Run the install script

The install script sets up the host: downloads Firecracker, builds the golden rootfs, configures TAP networking, and installs the systemd service.

```bash
sudo bash install.sh
```

This will:
- Install Firecracker v1.14.2 and jailer to `/usr/local/bin/`
- Download the AWS quickstart kernel to `/opt/fc-runner/vmlinux.bin`
- Debootstrap an Ubuntu 24.04 rootfs with the GitHub Actions runner pre-installed
- Create TAP device `tap-fc0` with NAT via iptables
- Copy config templates to `/etc/fc-runner/`
- Install and enable the `fc-runner.service` systemd unit

### 3. Configure

Edit `/etc/fc-runner/config.toml` with your GitHub PAT and repository:

```bash
sudo nano /etc/fc-runner/config.toml
```

At minimum, set:
- `github.token` — your PAT
- `github.owner` — repository owner
- `github.repo` — repository name

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
