# Guest Kernel Configuration

This document describes the kernel configuration items relevant for fc-runner
Firecracker microVMs. It covers the base Firecracker requirements plus the
additional modules needed for in-guest container networking (Podman/Docker).

Our kernel configs live in `guest_configs/` and are based on the
[official Firecracker configs](https://github.com/firecracker-microvm/firecracker/tree/main/resources/guest_configs),
built against the **Amazon Linux kernel source**
([`amazonlinux/linux`](https://github.com/amazonlinux/linux)) using tags like
`microvm-kernel-6.1.164-23.303.amzn2023`.

---

## Base Firecracker Configuration

These are the kernel options relevant for Firecracker, matching the
[upstream documentation](https://github.com/firecracker-microvm/firecracker/blob/main/docs/kernel-policy.md).

### Core I/O

| Config | Purpose |
|--------|---------|
| `CONFIG_SERIAL_8250_CONSOLE=y` | Serial console output |
| `CONFIG_PRINTK=y` | Kernel log messages |
| `CONFIG_BLK_DEV_INITRD=y` | initrd support |

### VirtIO Devices

| Config | Purpose |
|--------|---------|
| `CONFIG_VIRTIO_MMIO=y` | VirtIO over MMIO transport (default) |
| `CONFIG_VIRTIO_BLK=y` | Block devices (`/dev/vda`) |
| `CONFIG_VIRTIO_NET=y` | Network devices (`eth0`) |
| `CONFIG_VIRTIO_BALLOON=y` | Memory ballooning |
| `CONFIG_MEMORY_BALLOON=y` | Memory balloon infrastructure |
| `CONFIG_VIRTIO_VSOCKETS=y` | Host-guest VSOCK communication |
| `CONFIG_HW_RANDOM_VIRTIO=y` | Entropy from host |
| `CONFIG_RANDOM_TRUST_CPU=y` | Use CPU RNG to seed guest RNG (>= 5.10) |

### ACPI and Boot (x86_64)

| Config | Purpose |
|--------|---------|
| `CONFIG_ACPI=y` | ACPI support (required for x86_64 boot) |
| `CONFIG_KVM_GUEST=y` | KVM guest optimizations + `CONFIG_KVM_CLOCK` |
| `CONFIG_PTP_1588_CLOCK=y` | High precision timekeeping |
| `CONFIG_PTP_1588_CLOCK_KVM=y` | KVM PTP clock |

### Deprecated / Disabled

| Config | Status | Notes |
|--------|--------|-------|
| `CONFIG_X86_MPPARSE` | `not set` | Legacy MPTable — superseded by ACPI |
| `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` | `not set` | Legacy command-line VirtIO — superseded by ACPI |
| `CONFIG_SERIO` | `not set` | Not needed for Firecracker VMs |

### PCI Support

| Config | Status | Notes |
|--------|--------|-------|
| `CONFIG_PCI` | `not set` | **Not enabled** — see note below |

> **Note on CONFIG_PCI:** The upstream Firecracker docs state that `CONFIG_PCI`
> is needed for ACPI initialization on x86_64 when using mainline kernels.
> However, our kernel is built from **Amazon Linux's patched source**, which
> includes ACPI/VirtIO patches that allow ACPI boot without PCI. We keep PCI
> disabled and use `pci=off` in boot args. If you switch to a mainline kernel,
> you must enable `CONFIG_PCI=y` and the associated PCI configs.
>
> If you want PCI VirtIO transport (higher throughput), enable these and use
> `--enable-pci` when launching Firecracker:
> - `CONFIG_PCI=y`, `CONFIG_PCI_MMCONFIG=y`, `CONFIG_PCI_MSI=y`
> - `CONFIG_PCIEPORTBUS=y`, `CONFIG_VIRTIO_PCI=y`
> - `CONFIG_PCI_HOST_COMMON=y`, `CONFIG_PCI_HOST_GENERIC=y`
> - `CONFIG_BLK_MQ_PCI=y`
> - **Remove** `pci=off` from boot args

---

## Container Networking (fc-runner additions)

These configs are **not in the official Firecracker kernel config** but are
required by fc-runner for in-guest container networking with Podman/netavark.
GitHub Actions workflows use `services:` (e.g., `postgres:16`) which require
functional container port mapping inside the VM.

### Netfilter / iptables

| Config | Purpose |
|--------|---------|
| `CONFIG_IP_NF_IPTABLES=y` | iptables framework (in official config) |
| `CONFIG_IP_NF_NAT=y` | NAT support (in official config) |
| `CONFIG_IP_NF_TARGET_MASQUERADE=y` | MASQUERADE target (in official config) |
| `CONFIG_NF_NAT=y` | Core NAT (in official config) |
| `CONFIG_NF_NAT_MASQUERADE=y` | Masquerade helper (in official config) |
| `CONFIG_NF_NAT_REDIRECT=y` | Port redirect (in official config) |
| `CONFIG_NETFILTER_XT_MARK=y` | Packet mark matching — **fc-runner addition** |
| `CONFIG_NETFILTER_XT_TARGET_MARK=y` | MARK target (`-j MARK --set-xmark`) — **fc-runner addition** |
| `CONFIG_NETFILTER_XT_MATCH_COMMENT=y` | Rule comments (`-m comment`) — **fc-runner addition** |
| `CONFIG_NETFILTER_XT_MATCH_MULTIPORT=y` | Multi-port matching — **fc-runner addition** |
| `CONFIG_BRIDGE_NETFILTER=y` | Bridge netfilter (in official config) |

> **Why these are needed:** Podman's network backend (netavark) uses
> `iptables-nft` with `--set-xmark`, `-m comment`, and `-m multiport` for
> container port mapping and network isolation. Without these kernel modules,
> `podman run -p 5432:5432 postgres:16` fails with
> `Extension MARK revision 0 not supported`.

### Container Infrastructure

| Config | Purpose |
|--------|---------|
| `CONFIG_BRIDGE=y` | Bridge networking for containers |
| `CONFIG_VETH=y` | Virtual ethernet pairs (container ↔ bridge) |
| `CONFIG_OVERLAY_FS=y` | OverlayFS for container image layers |
| `CONFIG_SQUASHFS=y` | SquashFS support |
| `CONFIG_SQUASHFS_ZSTD=y` | Zstd-compressed squashfs |

### Namespaces and Cgroups

| Config | Purpose |
|--------|---------|
| `CONFIG_NAMESPACES=y` | Namespace support |
| `CONFIG_USER_NS=y` | User namespaces |
| `CONFIG_PID_NS=y` | PID namespaces |
| `CONFIG_NET_NS=y` | Network namespaces |
| `CONFIG_CGROUPS=y` | Control groups |
| `CONFIG_CGROUP_PIDS=y` | PID cgroup controller |
| `CONFIG_CGROUP_DEVICE=y` | Device cgroup controller |
| `CONFIG_CGROUP_CPUACCT=y` | CPU accounting |
| `CONFIG_CGROUP_FREEZER=y` | Freezer cgroup |
| `CONFIG_CGROUP_NET_PRIO=y` | Network priority cgroup |
| `CONFIG_CGROUP_NET_CLASSID=y` | Network classid cgroup |
| `CONFIG_CGROUP_BPF=y` | BPF cgroup |

---

## Minimal Boot Requirements

### Booting with root block device (our default)

For x86_64 with Amazon Linux kernel:

```
CONFIG_VIRTIO_BLK=y      # /dev/vda block device
CONFIG_ACPI=y             # ACPI tables
CONFIG_KVM_GUEST=y        # KVM optimizations
```

For x86_64 with mainline kernel, also add:

```
CONFIG_PCI=y              # Required for ACPI init on mainline
```

### Booting with initrd

```
CONFIG_BLK_DEV_INITRD=y
CONFIG_KVM_GUEST=y        # x86_64
CONFIG_VIRTIO_MMIO=y      # aarch64
```

### Boot logs (optional but recommended)

```
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_PRINTK=y
```

---

## Kernel Command Line

fc-runner uses the following default boot args:

```
console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda fsck.mode=skip quiet loglevel=3
```

| Parameter | Purpose |
|-----------|---------|
| `console=ttyS0` | Serial console for boot logs |
| `reboot=k` | Keyboard controller reset on reboot → clean `KVM_EXIT_SHUTDOWN` |
| `panic=1` | Reboot 1 second after kernel panic |
| `pci=off` | Skip PCI probing (no PCI devices in Firecracker MMIO mode) |
| `root=/dev/vda` | Root filesystem on VirtIO block device |
| `fsck.mode=skip` | Skip filesystem check (ephemeral VMs) |
| `quiet loglevel=3` | Reduce boot log noise |

Firecracker itself appends: `nomodule 8250.nr_uarts=0 i8042.noaux i8042.nomux
i8042.dumbkbd swiotlb=noforce`

> **Important:** If you enable PCI support (`--enable-pci`), you **must** remove
> `pci=off` from boot args. Firecracker only auto-appends `pci=off` when PCI is
> disabled.

---

## Building the Kernel

The kernel is built in CI from Amazon Linux's kernel source. See
`.github/workflows/release.yml` for the full build pipeline.

### Quick reference

```bash
# Clone Amazon Linux kernel
git clone --depth 1 --branch microvm-kernel-6.1.164-23.303.amzn2023 \
  https://github.com/amazonlinux/linux.git linux-amzn

# Apply config
cp guest_configs/microvm-kernel-ci-x86_64-6.1.config linux-amzn/.config
cd linux-amzn
make olddefconfig

# Build
make vmlinux -j$(nproc)

# Output: linux-amzn/vmlinux (ELF 64-bit)
```

### CI Verification

The release workflow verifies all required configs are present after
`make olddefconfig` (which can silently drop configs when the compiler
differs from the original):

- `CONFIG_SQUASHFS=y`, `CONFIG_SQUASHFS_ZSTD=y`
- `CONFIG_OVERLAY_FS=y`
- `CONFIG_VIRTIO=y`, `CONFIG_VIRTIO_BLK=y`, `CONFIG_VIRTIO_MMIO=y`, `CONFIG_VIRTIO_NET=y`
- `CONFIG_EXT4_FS=y`
- `CONFIG_VETH=y`, `CONFIG_BRIDGE=y`
- `CONFIG_IP_NF_IPTABLES=y`, `CONFIG_IP_NF_NAT=y`
- `CONFIG_ACPI=y`
- `CONFIG_NETFILTER_XT_MATCH_COMMENT=y`, `CONFIG_NETFILTER_XT_MATCH_MULTIPORT=y`
- `CONFIG_NETFILTER_XT_MARK=y`, `CONFIG_NETFILTER_XT_TARGET_MARK=y`

---

## Differences from Official Firecracker Config

Our kernel config is identical to the
[official Firecracker 6.1 x86_64 config](https://github.com/firecracker-microvm/firecracker/blob/main/resources/guest_configs/microvm-kernel-ci-x86_64-6.1.config)
except for **4 additions** in the netfilter section:

| Config | Official | fc-runner | Reason |
|--------|----------|-----------|--------|
| `CONFIG_NETFILTER_XT_MARK` | not set | `=y` | netavark `--set-xmark` |
| `CONFIG_NETFILTER_XT_TARGET_MARK` | not set | `=y` | netavark MARK target |
| `CONFIG_NETFILTER_XT_MATCH_COMMENT` | not set | `=y` | netavark rule comments |
| `CONFIG_NETFILTER_XT_MATCH_MULTIPORT` | not set | `=y` | netavark multi-port rules |

These are safe, additive changes that enable container networking inside the VM
without affecting any other Firecracker functionality.
