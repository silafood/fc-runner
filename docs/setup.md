# Setup Guide

## Prerequisites

- **Linux host** running Pop!_OS or Ubuntu 24.04 (bare-metal or nested virt enabled)
- **Rust toolchain** — install via [rustup](https://rustup.rs/)
- **GitHub credentials** — PAT or GitHub App (see below)

### GitHub Token Setup

fc-runner needs GitHub credentials to poll for queued jobs and register ephemeral runners. You can use a **Fine-grained PAT** (recommended), **Classic PAT**, or **GitHub App**.

#### Option A: Fine-grained PAT (recommended)

Fine-grained tokens provide least-privilege access scoped to specific repositories.

1. Go to **GitHub → Settings → Developer settings → Personal access tokens → Fine-grained tokens**
   (direct link: `https://github.com/settings/personal-access-tokens/new`)
2. Set a descriptive name (e.g., `fc-runner`)
3. Set **Expiration** — choose a reasonable period (90 days recommended, renew before expiry)
4. Under **Repository access**, select **Only select repositories** and pick **all** the repos you want fc-runner to serve (must match `repo`/`repos` in your config)
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

#### Option C: GitHub App (best for organizations)

GitHub Apps provide the highest rate limits (5,000/hour per installation), no token expiry management, and are preferred for organization deployments.

1. Go to **GitHub → Settings → Developer settings → GitHub Apps → New GitHub App**
2. Set a name (e.g., `fc-runner`) and homepage URL
3. Uncheck **Webhook → Active** (fc-runner polls, it doesn't use webhooks)
4. Under **Repository permissions**, grant:

   | Permission | Access | Why |
   |-----------|--------|-----|
   | **Actions** | Read and write | Poll queued runs/jobs, generate JIT runner tokens |
   | **Administration** | Read and write | Register ephemeral self-hosted runners |
   | **Metadata** | Read-only | Required by GitHub (auto-selected) |

5. Under **Where can this GitHub App be installed?**, select **Only on this account**
6. Click **Create GitHub App**
7. Note the **App ID** from the app settings page
8. Scroll down to **Private keys** and click **Generate a private key** — save the `.pem` file
9. Click **Install App** in the sidebar, then install it on the org/user that owns your repos
10. Note the **Installation ID** from the URL: `https://github.com/settings/installations/<ID>`

Configure in `config.toml`:
```toml
[github]
owner = "your-org"
repos = ["repo-a", "repo-b"]

[github.app]
app_id = 12345
installation_id = 67890
private_key_path = "/etc/fc-runner/app-key.pem"
```

Secure the private key:
```bash
sudo chmod 0600 /etc/fc-runner/app-key.pem
```

#### For Organization repositories

If fc-runner serves an **organization** repo:
- **PAT:** The org admin may need to approve the fine-grained PAT under **Organization settings → Personal access tokens → Pending requests**
- **GitHub App:** Install the App on the organization and ensure it has access to the required repos

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

After KVM is set up, fc-runner handles everything else automatically at startup:
- Downloads the guest kernel if missing
- Builds the golden rootfs from an Ubuntu cloud image if missing
- Creates per-VM TAP devices and configures NAT rules
- Loads AppArmor profiles if available

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
- Install system dependencies (curl, jq, iptables, etc.)
- Install Firecracker v1.14.2 and jailer to `/usr/local/bin/`
- Create directories and copy config templates to `/etc/fc-runner/`
- Install AppArmor profiles to `/etc/apparmor.d/`
- Install and enable the `fc-runner.service` systemd unit

Kernel download, rootfs building, and network setup are handled automatically by fc-runner at startup.

### 3. Configure

Edit `/etc/fc-runner/config.toml` with your GitHub PAT and repository:

```bash
sudo nano /etc/fc-runner/config.toml
```

At minimum, set:
- `github.token` (or `[github.app]`) — authentication credentials
- `github.owner` — repository owner
- `github.repo` — repository name (or `github.repos` for multiple)

**Single repo with PAT:**
```toml
[github]
token = "ghp_..."
owner = "your-org"
repo = "your-repo"
```

**Multiple repos with PAT:**
```toml
[github]
token = "ghp_..."
owner = "your-org"
repos = ["repo-one", "repo-two", "repo-three"]
```

**GitHub App (recommended for orgs):**
```toml
[github]
owner = "your-org"
repos = ["repo-one", "repo-two"]

[github.app]
app_id = 12345
installation_id = 67890
private_key_path = "/etc/fc-runner/app-key.pem"
```

**Org-level runners (cross-repo job pickup):**
```toml
[github]
token = "ghp_..."
owner = "your-org"
organization = "your-org"
repos = ["repo-one", "repo-two"]  # still needed for polling
```

Both `repo` and `repos` can be set — they are merged and deduplicated. All repos share the same token/app, labels, and runner group. When using fine-grained PATs, make sure the token has access to all listed repos.

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

# Option A: Run via systemd (production)
sudo systemctl start fc-runner
sudo journalctl -u fc-runner -f

# Option B: Run directly (development)
sudo fc-runner server --config /etc/fc-runner/config.toml

# Validate config without starting
fc-runner validate --config /etc/fc-runner/config.toml
```

On first startup, fc-runner will:
1. Verify KVM access
2. Download the guest kernel (~5 MB) if not present
3. Build the golden rootfs from an Ubuntu 24.04 cloud image (~2-3 minutes):
   - Download cloud image, convert qcow2 to raw, extract ext4 partition
   - Install packages (git, curl, jq, actions-runner) via chroot
   - Create runner user and entrypoint script
   - Shrink image to minimum size
4. Configure TAP networking and iptables NAT rules
5. Load AppArmor profiles
6. Begin polling GitHub for queued jobs

### 5. Trigger a workflow

Push a commit or manually trigger a workflow in your repo that uses:

```yaml
runs-on: [self-hosted, linux, firecracker]
```

fc-runner will pick up the job, boot a VM, and run it.

## Verify the Installation

```bash
# Validate config file
fc-runner validate --config /etc/fc-runner/config.toml

# Check service status
sudo systemctl status fc-runner

# Check Firecracker is installed
firecracker --version

# Check KVM is available
ls /dev/kvm

# Check golden rootfs exists (built automatically on first start)
ls -lh /opt/fc-runner/runner-rootfs-golden.ext4

# Check kernel exists (downloaded automatically on first start)
ls -lh /opt/fc-runner/vmlinux.bin

# List running VMs via CLI
fc-runner ps --endpoint http://localhost:9090

# List pools via CLI
fc-runner pools list --endpoint http://localhost:9090

# Check AppArmor profiles are enforced
sudo aa-status | grep -E '(firecracker|fc-runner)'

# Check COW reflink support (btrfs/xfs only; ext4 falls back to full copy)
df -Th /var/lib/fc-runner/vms
```

## Rebuilding the Golden Rootfs

The golden rootfs is built automatically on first startup. To force a rebuild (e.g., to update packages or the runner version):

```bash
# Check no VMs are running first
pgrep -x firecracker && echo "VMs running — wait" || echo "Safe to proceed"

# Delete the golden image and restart
sudo rm /opt/fc-runner/runner-rootfs-golden.ext4
sudo systemctl restart fc-runner
```

fc-runner will detect the missing rootfs and rebuild it automatically.

## Updating AppArmor Profiles

After updating the AppArmor profile files in the repo:

```bash
sudo cp apparmor/usr.local.bin.fc-runner /etc/apparmor.d/
sudo cp apparmor/usr.local.bin.firecracker /etc/apparmor.d/
sudo apparmor_parser -r /etc/apparmor.d/usr.local.bin.fc-runner
sudo apparmor_parser -r /etc/apparmor.d/usr.local.bin.firecracker
```
