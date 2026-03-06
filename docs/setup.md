# Setup Guide

## Prerequisites

- **Linux host** running Pop!_OS or Ubuntu 24.04 (bare-metal or nested virt enabled)
- **Rust toolchain** — install via [rustup](https://rustup.rs/)
- **GitHub Personal Access Token (PAT)** — see below for how to generate one

### GitHub Token Setup

fc-runner needs a GitHub token to poll for queued jobs and register ephemeral runners. You can use either a **Fine-grained PAT** (recommended) or a **Classic PAT**.

#### Option A: Fine-grained PAT (recommended)

Fine-grained tokens provide least-privilege access scoped to specific repositories.

1. Go to **GitHub → Settings → Developer settings → Personal access tokens → Fine-grained tokens**
   (direct link: `https://github.com/settings/personal-access-tokens/new`)
2. Set a descriptive name (e.g., `fc-runner`)
3. Set **Expiration** — choose a reasonable period (90 days recommended, renew before expiry)
4. Under **Repository access**, select **Only select repositories** and pick the repo(s) you want fc-runner to serve
5. Under **Permissions → Repository permissions**, grant:

   | Permission | Access | Why |
   |-----------|--------|-----|
   | **Actions** | Read and write | Poll queued runs/jobs, generate JIT runner tokens |
   | **Administration** | Read and write | Register ephemeral self-hosted runners |
   | **Metadata** | Read-only | Required by GitHub (auto-selected) |

6. Click **Generate token** and copy it immediately — you won't see it again

#### Option B: Classic PAT

Classic tokens are simpler but grant broader access.

1. Go to **GitHub → Settings → Developer settings → Personal access tokens → Tokens (classic)**
   (direct link: `https://github.com/settings/tokens/new`)
2. Set a descriptive name (e.g., `fc-runner`)
3. Set **Expiration**
4. Select the **`repo`** scope (full control of private repositories)

   > The `repo` scope is required because the Actions runner registration API needs write access. There is no narrower classic scope available.

5. Click **Generate token** and copy it

#### For Organization repositories

If fc-runner serves an **organization** repo, the org admin may need to:
- Approve the fine-grained PAT under **Organization settings → Personal access tokens → Pending requests**
- Or use a **GitHub App** installation token instead of a PAT (not covered here)

#### Security best practices

- Store the token only in `/etc/fc-runner/config.toml` with restricted permissions:
  ```bash
  sudo chmod 0600 /etc/fc-runner/config.toml
  ```
- fc-runner uses `secrecy::SecretString` internally — the token is zeroized from memory on drop and never appears in logs
- Rotate tokens regularly and use the shortest practical expiration
- Prefer fine-grained PATs scoped to specific repos over classic tokens

#### Verify your token works

```bash
# Test the token (replace values)
curl -s \
  -H "Authorization: Bearer ghp_your_token_here" \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  https://api.github.com/repos/OWNER/REPO/actions/runners \
  | jq '.total_count'
```

If you get a number (even `0`), the token has the right permissions. If you get `401` or `403`, check the scopes.

### KVM Setup (one-time, requires sudo)

fc-runner checks KVM access automatically at startup and will provide clear error messages if something is missing. Here's how to set it up:

```bash
# 1. Verify CPU supports virtualization (must return > 0)
grep -Eoc '(vmx|svm)' /proc/cpuinfo

# 2. Detect CPU vendor and load the correct KVM module
CPU_VENDOR=$(lscpu | awk -F: '/Vendor ID/{gsub(/^ +/,"",$2); print $2}')
if [ "$CPU_VENDOR" = "GenuineIntel" ]; then
    echo "Intel CPU detected"
    sudo modprobe kvm_intel
elif [ "$CPU_VENDOR" = "AuthenticAMD" ]; then
    echo "AMD CPU detected"
    sudo modprobe kvm_amd
else
    echo "Unknown CPU vendor: $CPU_VENDOR"
    exit 1
fi

# 3. Verify /dev/kvm exists
ls -la /dev/kvm
# Should show: crw-rw---- 1 root kvm ...

# 4. Add your user to the kvm group (avoids needing root for KVM access)
sudo usermod -aG kvm $USER
# Apply the new group (or log out and back in)
newgrp kvm
```

> **Note:** `newgrp kvm` starts a new shell to apply the group immediately. If it
> causes issues with your shell profile, simply log out and back in instead.

You can also check manually:
```bash
# Quick check: "vmx" = Intel, "svm" = AMD
grep -Eoc 'vmx' /proc/cpuinfo   # Intel (VT-x)
grep -Eoc 'svm' /proc/cpuinfo   # AMD (AMD-V)

# Or use lscpu
lscpu | grep "Vendor ID"
# GenuineIntel → use kvm_intel
# AuthenticAMD → use kvm_amd
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
