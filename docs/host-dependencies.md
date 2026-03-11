# Host Dependencies

This document lists all system tools and packages required to run fc-runner on a Linux host.

## Quick Install

### Ubuntu/Debian

```bash
# All required packages in one command
sudo apt-get update && sudo apt-get install -y \
    curl wget \
    e2fsprogs \
    mount \
    iptables ipset iproute2 \
    squashfs-tools \
    debootstrap \
    jq
```

The `install.sh` script handles this automatically.

### Arch Linux

```bash
# All required packages in one command
# Note: iptables-nft and iptables conflict — install whichever your system doesn't already have
sudo pacman -Sy --noconfirm --needed \
    curl wget \
    e2fsprogs \
    iptables-nft ipset iproute2 \
    squashfs-tools \
    debootstrap \
    jq unzip
```

The `install-arch.sh` script handles this automatically.

---

## Required Tools

### Core Runtime (always needed)

These are used by fc-runner during normal operation:

| Tool | Package | Used By | Purpose |
|------|---------|---------|---------|
| `mount` / `umount` | `mount` (usually pre-installed) | `firecracker.rs`, `setup.rs`, `image.rs` | Loop-mount ext4 images to inject network config, read guest logs |
| `mkfs.ext4` | `e2fsprogs` | `firecracker.rs`, `image.rs` | Format per-VM overlay ext4 and OCI rootfs images |
| `cp` | `coreutils` (pre-installed) | `firecracker.rs` | COW-copy golden rootfs (`--reflink=auto --sparse=always`) — legacy mode only |
| `iptables` | `iptables` | `setup.rs` | NAT masquerade rules for VM internet access |
| `ipset` | `ipset` | `setup.rs` | IP set management for network allowlisting |
| `firecracker` | [Firecracker release](https://github.com/firecracker-microvm/firecracker/releases) | `firecracker.rs` | The microVM hypervisor — installed by `install.sh` |
| `jailer` | Bundled with Firecracker | `firecracker.rs` | Chroot + seccomp-BPF isolation for VMs (optional but recommended) |

```bash
# Ubuntu/Debian
sudo apt-get install -y e2fsprogs iptables ipset iproute2 mount

# Arch Linux
sudo pacman -S --needed e2fsprogs iptables-nft ipset iproute2
```

### OverlayFS Mode (default, recommended)

Required when `overlay_rootfs = true` (the default):

| Tool | Package | Used By | Purpose |
|------|---------|---------|---------|
| `mksquashfs` | `squashfs-tools` | `setup.rs` | Convert golden ext4 rootfs to compressed squashfs (zstd) |

```bash
# Ubuntu/Debian
sudo apt-get install -y squashfs-tools

# Arch Linux
sudo pacman -S --needed squashfs-tools
```

fc-runner checks for `mksquashfs` at startup and gives a clear error if missing. To disable overlay mode and skip this dependency, set `overlay_rootfs = false` in your config.

### Golden Rootfs Build (first startup)

Used once to build the golden rootfs from an Ubuntu cloud image. After the rootfs is built, these are only needed again if you delete and rebuild it.

| Tool | Package | Used By | Purpose |
|------|---------|---------|---------|
| `e2fsck` | `e2fsprogs` | `setup.rs` | Check and repair ext4 before shrinking |
| `resize2fs` | `e2fsprogs` | `setup.rs` | Shrink ext4 image to minimum size + headroom |
| `chroot` | `coreutils` (pre-installed) | `setup.rs` | Run commands inside the rootfs (apt-get, useradd, etc.) |
| `curl` | `curl` | `setup.rs` | Download Ubuntu cloud image and GitHub Actions runner |
| `debootstrap` | `debootstrap` | `build-rootfs.sh` | Bootstrap Ubuntu rootfs (alternative build script) |

```bash
# Ubuntu/Debian
sudo apt-get install -y e2fsprogs curl debootstrap

# Arch Linux
sudo pacman -S --needed e2fsprogs curl debootstrap
```

### Network Management

| Tool | Package | Used By | Purpose |
|------|---------|---------|---------|
| `iptables` | `iptables` | `setup.rs` | MASQUERADE NAT + TCP MSS clamping for VM traffic |
| `ipset` | `ipset` | `setup.rs` | IP set for network allowlisting (`allowed_networks` config) |

```bash
# Ubuntu/Debian
sudo apt-get install -y iptables ipset

# Arch Linux
sudo pacman -S --needed iptables-nft ipset
```

> **Note:** TAP device creation and IP assignment use pure Rust (`nix` crate ioctl + `rtnetlink` crate) — no `ip` command needed at runtime.

### Manual Build Script (`build-v611-linux.sh`)

Only needed if you build the golden rootfs manually instead of letting fc-runner auto-provision:

| Tool | Package | Used By | Purpose |
|------|---------|---------|---------|
| `qemu-img` | `qemu-utils` | `build-v611-linux.sh` | Convert qcow2 cloud image to raw |
| `losetup` | `mount` (pre-installed) | `build-v611-linux.sh` | Attach raw image as loop device with partition scanning |
| `blkid` | `util-linux` (pre-installed) | `build-v611-linux.sh` | Identify ext4 partition in raw image |
| `dd` | `coreutils` (pre-installed) | `build-v611-linux.sh` | Extract partition from raw image |
| `truncate` | `coreutils` (pre-installed) | `build-v611-linux.sh` | Create/resize sparse files |
| `dumpe2fs` | `e2fsprogs` | `build-v611-linux.sh` | Read ext4 superblock info (block count/size) |
| `tar` | `tar` (pre-installed) | `build-v611-linux.sh` | Extract GitHub Actions runner tarball |

```bash
# Ubuntu/Debian
sudo apt-get install -y qemu-utils e2fsprogs

# Arch Linux
sudo pacman -S --needed qemu-img e2fsprogs
```

---

## Kernel Requirements

fc-runner uses a custom minimal Linux 6.1.102 kernel compiled with Firecracker's config. The kernel is **downloaded automatically** on first startup from the GitHub release.

Key kernel configs (already enabled in the provided kernel):

| Config | Purpose |
|--------|---------|
| `CONFIG_VIRTIO=y` | Firecracker virtio devices |
| `CONFIG_EXT4_FS=y` | Root filesystem |
| `CONFIG_KVM_GUEST=y` | KVM paravirtualization |
| `CONFIG_OVERLAY_FS=y` | OverlayFS COW rootfs mode |
| `CONFIG_SQUASHFS=y` | Read-only compressed rootfs base |
| `CONFIG_SQUASHFS_ZSTD=y` | Zstd compression for squashfs |

You do **not** need to compile a kernel — it's downloaded from the fc-runner GitHub release.

---

## KVM Access

Firecracker requires hardware virtualization (KVM):

```bash
# Verify CPU supports virtualization (must return > 0)
grep -Eoc '(vmx|svm)' /proc/cpuinfo

# Load KVM module
sudo modprobe kvm_intel   # Intel
sudo modprobe kvm_amd     # AMD

# Add your user to the kvm group
sudo usermod -aG kvm $USER
newgrp kvm
```

---

## Firecracker Installation

The `install.sh` script handles this, but to install manually:

```bash
FC_VERSION="1.14.2"
ARCH=$(uname -m)
curl -sL "https://github.com/firecracker-microvm/firecracker/releases/download/v${FC_VERSION}/firecracker-v${FC_VERSION}-${ARCH}.tgz" | tar xz -C /tmp
sudo install -m 0755 /tmp/release-v${FC_VERSION}-${ARCH}/firecracker-v${FC_VERSION}-${ARCH} /usr/local/bin/firecracker
sudo install -m 0755 /tmp/release-v${FC_VERSION}-${ARCH}/jailer-v${FC_VERSION}-${ARCH} /usr/local/bin/jailer
```

Verify:

```bash
firecracker --version
# firecracker v1.14.2
```

---

## Guest-Side Tools (inside VM)

These are installed **inside the golden rootfs** during build — not on the host. Listed here for reference:

| Tool | Purpose |
|------|---------|
| `git`, `curl`, `jq`, `ca-certificates` | GitHub Actions runner dependencies |
| `sudo` | Runner user privilege escalation |
| `iproute2`, `systemd-resolved` | Guest networking |
| `build-essential`, `gcc`, `g++`, `make`, `cmake` | Build tools for CI jobs |
| `python3`, `nodejs`, `npm` | Language runtimes |
| `docker.io`, `containerd` | Container support in CI jobs |
| GitHub Actions Runner v2.332.0 | The actual job executor |
| `fc-runner` (agent mode) | Reads MMDS, starts runner, reports via VSOCK |

---

## Verification Checklist

After installing all dependencies, verify with:

```bash
# Core tools
which mount mkfs.ext4 iptables firecracker
# → should print paths for all

# OverlayFS mode (default)
which mksquashfs
# → /usr/bin/mksquashfs

# KVM access
ls -la /dev/kvm
# → crw-rw---- 1 root kvm ...

# Firecracker version
firecracker --version
# → firecracker v1.14.2

# Validate config
fc-runner validate --config /etc/fc-runner/config.toml
```

---

## Troubleshooting

| Error | Cause | Fix |
|-------|-------|-----|
| `mksquashfs not found` | `squashfs-tools` not installed | Ubuntu: `sudo apt install squashfs-tools` / Arch: `sudo pacman -S squashfs-tools` |
| `KVM not available` | Missing kernel module | `sudo modprobe kvm_intel` (or `kvm_amd`) |
| `Permission denied on /dev/kvm` | User not in kvm group | `sudo usermod -aG kvm $USER && newgrp kvm` |
| `mount: /dev/loop*: failed` | Loop devices exhausted | `sudo modprobe loop max_loop=64` |
| `mkfs.ext4: not found` | `e2fsprogs` not installed | Ubuntu: `sudo apt install e2fsprogs` / Arch: `sudo pacman -S e2fsprogs` |
| `iptables: not found` | `iptables` not installed | Ubuntu: `sudo apt install iptables` / Arch: `sudo pacman -S iptables-nft` |
| `firecracker: not found` | Firecracker not installed | Run `sudo bash install.sh` (Ubuntu) or `sudo bash install-arch.sh` (Arch) |
