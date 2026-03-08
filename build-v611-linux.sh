#!/usr/bin/env bash
# Build golden rootfs using Ubuntu minimal cloud image + Linux 6.1.102 kernel.
# Run this manually with sudo BEFORE starting fc-runner.
# ~1 minute build time.
set -euo pipefail

ROOTFS="/opt/fc-runner/runner-rootfs-golden.ext4"
KERNEL="/opt/fc-runner/vmlinux.bin"
MNT="/opt/fc-runner/rootfs-build"
RUNNER_VERSION="2.332.0"
KERNEL_URL="https://github.com/silafood/fc-runner/releases/download/v0.1.0/vmlinux-6.1.102"
CLOUD_IMG_URL="https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-amd64.img"
CLOUD_IMG="/opt/fc-runner/cloud-base.img"

echo "=== Building golden rootfs + kernel (v6.1.102) ==="

# ── Step 0: Download kernel ──────────────────────────────────────────
if [ -f "$KERNEL" ]; then
    echo "[0/8] Using cached kernel"
else
    echo "[0/8] Downloading Linux 6.1.102 kernel..."
    curl -fSL -o "$KERNEL" "$KERNEL_URL"
fi
echo "  Kernel: $(ls -lh "$KERNEL" | awk '{print $5}')"

# Clean up any previous failed build
umount -l "$MNT" 2>/dev/null || true
rm -rf "$MNT"
rm -f "$ROOTFS"

# ── Step 1: Download cloud image (cached) ────────────────────────────
if [ -f "$CLOUD_IMG" ]; then
    echo "[1/8] Using cached cloud image"
else
    echo "[1/8] Downloading Ubuntu minimal cloud image..."
    curl -fSL -o "$CLOUD_IMG" "$CLOUD_IMG_URL"
fi

# ── Step 2: Extract ext4 partition from cloud image ──────────────────
echo "[2/8] Extracting ext4 partition from cloud image..."
RAWIMG="/opt/fc-runner/cloud-raw.img"
qemu-img convert -f qcow2 -O raw "$CLOUD_IMG" "$RAWIMG"

# Attach with partition scanning, extract the root partition (p1)
LOOP=$(losetup --find --show --partscan "$RAWIMG")
# Find the ext4 partition (usually p1, but check)
PART=""
for p in "${LOOP}p1" "${LOOP}p2" "${LOOP}p15"; do
    if [ -b "$p" ] && blkid "$p" 2>/dev/null | grep -q ext4; then
        PART="$p"
        break
    fi
done
if [ -z "$PART" ]; then
    echo "ERROR: No ext4 partition found in cloud image"
    losetup -d "$LOOP"
    exit 1
fi
echo "  Found ext4 partition: $PART"
dd if="$PART" of="$ROOTFS" bs=4M status=progress
losetup -d "$LOOP"
rm -f "$RAWIMG"

# Expand to 4GB to ensure it's larger than the source partition
truncate -s 6G "$ROOTFS"
e2fsck -f -y "$ROOTFS" || true
resize2fs "$ROOTFS"

# ── Step 3: Mount ────────────────────────────────────────────────────
echo "[3/8] Mounting..."
mkdir -p "$MNT"
mount -o loop "$ROOTFS" "$MNT"

cleanup() {
    echo "Cleaning up mounts..."
    umount "$MNT/dev/pts" 2>/dev/null || true
    umount "$MNT/dev" 2>/dev/null || true
    umount "$MNT/proc" 2>/dev/null || true
    umount "$MNT/sys" 2>/dev/null || true
    umount -l "$MNT" 2>/dev/null || true
    rmdir "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

# Mount pseudo-filesystems
mount --bind /dev "$MNT/dev"
mount --bind /dev/pts "$MNT/dev/pts"
mount -t proc proc "$MNT/proc"
mount -t sysfs sys "$MNT/sys"

# Fix fstab: cloud image may have wrong UUID for Firecracker's /dev/vda
printf '/dev/vda\t/\text4\tdefaults,noatime\t0\t1\n' > "$MNT/etc/fstab"

# ── Step 4: Fix DNS in chroot ────────────────────────────────────────
echo "[4/8] Configuring DNS for chroot..."
# Cloud image has resolv.conf as a symlink to systemd-resolved stub — remove it
rm -f "$MNT/etc/resolv.conf"
printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > "$MNT/etc/resolv.conf"

# ── Step 5: Install only what's missing ──────────────────────────────
echo "[5/8] Installing runner dependencies and build tools..."
chroot "$MNT" bash -c "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -q
    apt-get install -y --no-install-recommends \
        curl git jq ca-certificates sudo libicu74 iproute2 systemd-resolved \
        build-essential pkg-config libssl-dev \
        gcc g++ make cmake \
        python3 python3-pip python3-venv \
        nodejs npm \
        docker.io containerd \
        wget tar gzip xz-utils \
        zip bzip2 \
        libffi-dev zlib1g-dev \
        net-tools dnsutils iputils-ping \
        locales
    apt-get clean
    rm -rf /var/lib/apt/lists/*
"

echo "[5b/8] Installing Rust toolchain..."
chroot "$MNT" bash -c "
    su - runner -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable'
    ln -sf /home/runner/.cargo/bin/cargo /usr/local/bin/cargo
    ln -sf /home/runner/.cargo/bin/rustc /usr/local/bin/rustc
    ln -sf /home/runner/.cargo/bin/rustup /usr/local/bin/rustup
    echo 'export PATH=/home/runner/.cargo/bin:\$PATH' >> /home/runner/.bashrc
    echo 'PATH=/home/runner/.cargo/bin:/usr/local/bin:/usr/bin:/bin' > /etc/environment
"

# Ensure /var/tmp exists (systemd-resolved needs it for PrivateTmp namespace)
mkdir -p "$MNT/var/tmp"
chmod 1777 "$MNT/var/tmp"

# Restore systemd-resolved symlink
rm -f "$MNT/etc/resolv.conf"
ln -s /run/systemd/resolve/stub-resolv.conf "$MNT/etc/resolv.conf"

# ── Step 6: Configure network + runner user ──────────────────────────
echo "[6/8] Configuring network and runner user..."
mkdir -p "$MNT/etc/systemd/network"
cat > "$MNT/etc/systemd/network/20-eth.network" << 'EOF'
[Match]
Name=eth0

[Network]
Address=172.16.0.2/24
Gateway=172.16.0.1
DNS=8.8.8.8
DNS=1.1.1.1
EOF

chroot "$MNT" systemctl enable systemd-networkd systemd-resolved 2>/dev/null || true

# Belt-and-suspenders: create symlinks manually in case chroot systemctl fails
mkdir -p "$MNT/etc/systemd/system/multi-user.target.wants"
ln -sf /lib/systemd/system/systemd-networkd.service "$MNT/etc/systemd/system/multi-user.target.wants/systemd-networkd.service"
ln -sf /lib/systemd/system/systemd-resolved.service "$MNT/etc/systemd/system/multi-user.target.wants/systemd-resolved.service"

# Disable services that slow boot and aren't needed
chroot "$MNT" bash -c "
    systemctl disable apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true
    systemctl disable motd-news.timer 2>/dev/null || true
    systemctl mask systemd-timesyncd.service 2>/dev/null || true
" 2>/dev/null || true

chroot "$MNT" useradd -m -s /bin/bash runner 2>/dev/null || true
echo "runner ALL=(ALL) NOPASSWD:ALL" > "$MNT/etc/sudoers.d/runner"

# ── Step 7: Install GitHub Actions runner ────────────────────────────
echo "[7/8] Installing GitHub Actions runner v${RUNNER_VERSION}..."
curl -fsSL -o "$MNT/home/runner/actions-runner.tar.gz" \
    "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
tar xzf "$MNT/home/runner/actions-runner.tar.gz" -C "$MNT/home/runner/"
rm "$MNT/home/runner/actions-runner.tar.gz"
chroot "$MNT" chown -R runner:runner /home/runner

# ── Step 8: Write entrypoint ─────────────────────────────────────────
echo "[8/8] Writing entrypoint..."
cat > "$MNT/entrypoint.sh" << 'ENTRYEOF'
#!/bin/bash
set -euo pipefail
exec > /var/log/runner.log 2>&1

echo "=== fc-runner entrypoint $(date) ==="

if [ ! -f /etc/fc-runner-env ]; then
    echo "ERROR: /etc/fc-runner-env not found"
    sleep 3
    reboot -f
fi

source /etc/fc-runner-env
echo "VM_ID=${VM_ID} MODE=${RUNNER_MODE:-jit}"

# Wait for network connectivity
for i in $(seq 1 30); do
    if ip route show default | grep -q default 2>/dev/null; then
        if curl -sf --connect-timeout 3 --max-time 5 https://github.com > /dev/null 2>&1; then
            echo "Network ready"
            break
        fi
    fi
    echo "Waiting for network ($i/30)..."
    sleep 1
done

cd /home/runner

if [ "${RUNNER_MODE:-jit}" = "jit" ]; then
    echo "Starting runner (JIT mode)..."
    sudo -E -u runner ./run.sh --jitconfig "${RUNNER_TOKEN}"
else
    echo "Registering runner..."
    sudo -E -u runner ./config.sh \
        --url "${REPO_URL}" \
        --token "${RUNNER_TOKEN}" \
        --name "${RUNNER_NAME:-fc-$(hostname)}" \
        --labels "firecracker,linux,x64" \
        --ephemeral \
        --unattended \
        --disableupdate \
        --work /home/runner/_work
    echo "Starting runner (registered mode)..."
    sudo -E -u runner ./run.sh
fi

echo "Runner finished, shutting down"
reboot -f
ENTRYEOF
chmod +x "$MNT/entrypoint.sh"

# Create rc-local.service unit (not shipped in Ubuntu 24.04 cloud images)
cat > "$MNT/etc/systemd/system/rc-local.service" << 'SVCEOF'
[Unit]
Description=/etc/rc.local Compatibility
ConditionFileIsExecutable=/etc/rc.local

[Service]
Type=forking
ExecStart=/etc/rc.local
TimeoutSec=0
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
SVCEOF

chroot "$MNT" systemctl enable rc-local.service 2>/dev/null || true
# Manual symlink fallback
ln -sf /etc/systemd/system/rc-local.service "$MNT/etc/systemd/system/multi-user.target.wants/rc-local.service"

cat > "$MNT/etc/rc.local" << 'RCEOF'
#!/bin/bash
/entrypoint.sh &
exit 0
RCEOF
chmod +x "$MNT/etc/rc.local"

# ── Finalize ─────────────────────────────────────────────────────────
umount "$MNT/dev/pts"
umount "$MNT/dev"
umount "$MNT/proc"
umount "$MNT/sys"
umount "$MNT"
rmdir "$MNT"
trap - EXIT

# Shrink image to actual usage + 512MB headroom
echo "Shrinking image..."
e2fsck -f -y "$ROOTFS" || true
resize2fs -M "$ROOTFS"
MINBLOCKS=$(dumpe2fs -h "$ROOTFS" 2>/dev/null | grep "Block count" | awk '{print $3}')
BLOCKSIZE=$(dumpe2fs -h "$ROOTFS" 2>/dev/null | grep "Block size" | awk '{print $3}')
MINBYTES=$((MINBLOCKS * BLOCKSIZE))
FINALBYTES=$((MINBYTES + 512 * 1024 * 1024))
resize2fs "$ROOTFS" "$((FINALBYTES / BLOCKSIZE))" 2>/dev/null || true
truncate -s "$FINALBYTES" "$ROOTFS"

echo ""
echo "=== Golden rootfs ready at $ROOTFS ==="
ls -lh "$ROOTFS"
echo "=== Kernel ready at $KERNEL ==="
ls -lh "$KERNEL"
