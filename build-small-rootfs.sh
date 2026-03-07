#!/usr/bin/env bash
# Fast rootfs using Ubuntu minimal cloud image instead of debootstrap.
# ~1 minute build time. Boots faster and has better systemd integration.
set -euo pipefail

ROOTFS="/opt/fc-runner/runner-rootfs-golden.ext4"
MNT="/opt/fc-runner/rootfs-build"
RUNNER_VERSION="2.332.0"
CLOUD_IMG_URL="https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-amd64.img"
CLOUD_IMG="/opt/fc-runner/cloud-base.img"

echo "=== Building golden rootfs (cloud image base) ==="

# Clean up any previous failed build
umount -l "$MNT" 2>/dev/null || true
rm -rf "$MNT"
rm -f "$ROOTFS"

# ── Step 1: Download cloud image (cached) ──────────────────────────
if [ -f "$CLOUD_IMG" ]; then
    echo "[1/8] Using cached cloud image"
else
    echo "[1/8] Downloading Ubuntu minimal cloud image..."
    curl -fSL -o "$CLOUD_IMG" "$CLOUD_IMG_URL"
fi

# ── Step 2: Convert qcow2 → raw ext4 ───────────────────────────────
echo "[2/8] Converting qcow2 → raw ext4 (2GB)..."
qemu-img convert -f qcow2 -O raw "$CLOUD_IMG" "$ROOTFS"
# Expand to 2GB so there's room for the runner + workspace
truncate -s 2G "$ROOTFS"
e2fsck -f -y "$ROOTFS" || true
resize2fs "$ROOTFS"

# ── Step 3: Mount ───────────────────────────────────────────────────
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

# ── Step 4: Strip cloud-init and unnecessary services ───────────────
echo "[4/8] Stripping cloud-init and unnecessary packages..."
chroot "$MNT" bash -c "
    export DEBIAN_FRONTEND=noninteractive
    apt-get -y purge cloud-init cloud-guest-utils snapd lxd-installer \
        ubuntu-advantage-tools unattended-upgrades 2>/dev/null || true
    apt-get -y autoremove --purge 2>/dev/null || true
    rm -rf /var/lib/cloud /etc/cloud /var/cache/apt/archives/*.deb
    apt-get clean
"

# ── Step 5: Install only what we need ───────────────────────────────
echo "[5/8] Installing runner dependencies..."
chroot "$MNT" bash -c "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -q
    apt-get install -y --no-install-recommends \
        curl git jq ca-certificates sudo libicu74 iproute2
    apt-get clean
    rm -rf /var/lib/apt/lists/*
"

# ── Step 6: Configure network + runner user ─────────────────────────
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

chroot "$MNT" systemctl enable systemd-networkd systemd-resolved

# Disable services that slow boot and aren't needed
chroot "$MNT" bash -c "
    systemctl disable apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true
    systemctl disable motd-news.timer 2>/dev/null || true
    systemctl mask systemd-timesyncd.service 2>/dev/null || true
" 2>/dev/null || true

chroot "$MNT" useradd -m -s /bin/bash runner 2>/dev/null || true
echo "runner ALL=(ALL) NOPASSWD:ALL" > "$MNT/etc/sudoers.d/runner"

# ── Step 7: Install GitHub Actions runner ───────────────────────────
echo "[7/8] Installing GitHub Actions runner v${RUNNER_VERSION}..."
curl -fsSL -o "$MNT/home/runner/actions-runner.tar.gz" \
    "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
tar xzf "$MNT/home/runner/actions-runner.tar.gz" -C "$MNT/home/runner/"
rm "$MNT/home/runner/actions-runner.tar.gz"
chroot "$MNT" chown -R runner:runner /home/runner

# ── Step 8: Write entrypoint ────────────────────────────────────────
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
    sudo -u runner ./run.sh --jitconfig "${RUNNER_TOKEN}"
else
    echo "Registering runner..."
    sudo -u runner ./config.sh \
        --url "${REPO_URL}" \
        --token "${RUNNER_TOKEN}" \
        --name "${RUNNER_NAME:-fc-$(hostname)}" \
        --labels "firecracker,linux,x64" \
        --ephemeral \
        --unattended \
        --disableupdate \
        --work /home/runner/_work
    echo "Starting runner (registered mode)..."
    sudo -u runner ./run.sh
fi

echo "Runner finished, shutting down"
reboot -f
ENTRYEOF
chmod +x "$MNT/entrypoint.sh"

chroot "$MNT" systemctl enable rc-local.service 2>/dev/null || true

cat > "$MNT/etc/rc.local" << 'RCEOF'
#!/bin/bash
/entrypoint.sh &
exit 0
RCEOF
chmod +x "$MNT/etc/rc.local"

# ── Finalize ────────────────────────────────────────────────────────
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
