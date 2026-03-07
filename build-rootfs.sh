#!/usr/bin/env bash
set -euo pipefail

ROOTFS="/opt/fc-runner/runner-rootfs-golden.ext4"
MNT="/opt/fc-runner/rootfs-build"
RUNNER_VERSION="2.332.0"

echo "=== Building golden rootfs ==="

# Clean up any previous failed build
umount -l "$MNT" 2>/dev/null || true
rm -rf "$MNT"
rm -f "$ROOTFS"

echo "[1/7] Creating 2GB sparse image..."
truncate -s 2G "$ROOTFS"

echo "[2/7] Formatting ext4..."
mkfs.ext4 -F "$ROOTFS"

echo "[3/7] Mounting..."
mkdir -p "$MNT"
mount -o loop "$ROOTFS" "$MNT"

# Ensure cleanup on exit (unmount pseudo-fs first, then rootfs)
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

echo "[4/7] Running debootstrap (minbase variant)..."
debootstrap --arch=amd64 --variant=minbase \
    --include=systemd,systemd-sysv,curl,git,jq,ca-certificates,sudo,openssh-client,unzip,libicu74,iproute2,iputils-ping \
    noble "$MNT" http://archive.ubuntu.com/ubuntu

# Mount pseudo-filesystems needed by chroot commands
mount --bind /dev "$MNT/dev"
mount --bind /dev/pts "$MNT/dev/pts"
mount -t proc proc "$MNT/proc"
mount -t sysfs sys "$MNT/sys"

echo "[5/7] Configuring network..."
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

echo "[6/7] Creating runner user and installing GitHub Actions runner..."
chroot "$MNT" useradd -m -s /bin/bash runner || true
echo "runner ALL=(ALL) NOPASSWD:ALL" > "$MNT/etc/sudoers.d/runner"

curl -fsSL -o "$MNT/home/runner/actions-runner.tar.gz" \
    "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
tar xzf "$MNT/home/runner/actions-runner.tar.gz" -C "$MNT/home/runner/"
rm "$MNT/home/runner/actions-runner.tar.gz"

# Skip installdependencies.sh — it pulls in hundreds of MB of packages
# (dotnet, docker, etc.) that aren't needed. libicu74 from debootstrap
# is the only real runtime dependency for the runner binary.
# Uncomment the next line if workflows need build tools:
# chroot "$MNT" /home/runner/bin/installdependencies.sh
chroot "$MNT" chown -R runner:runner /home/runner

echo "[7/7] Writing entrypoint..."
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

# Enable rc-local.service explicitly
chroot "$MNT" systemctl enable rc-local.service 2>/dev/null || true

cat > "$MNT/etc/rc.local" << 'RCEOF'
#!/bin/bash
/entrypoint.sh &
exit 0
RCEOF
chmod +x "$MNT/etc/rc.local"

# Unmount pseudo-filesystems, then rootfs
umount "$MNT/dev/pts"
umount "$MNT/dev"
umount "$MNT/proc"
umount "$MNT/sys"
umount "$MNT"
rmdir "$MNT"
trap - EXIT

echo "[8/8] Shrinking image to minimum size..."
e2fsck -f -y "$ROOTFS" || true
resize2fs -M "$ROOTFS"
# Pad 512MB headroom for runtime writes (runner work dir, logs, etc.)
MINBLOCKS=$(dumpe2fs -h "$ROOTFS" 2>/dev/null | grep "Block count" | awk '{print $3}')
BLOCKSIZE=$(dumpe2fs -h "$ROOTFS" 2>/dev/null | grep "Block size" | awk '{print $3}')
MINBYTES=$((MINBLOCKS * BLOCKSIZE))
FINALBYTES=$((MINBYTES + 512 * 1024 * 1024))
resize2fs "$ROOTFS" "$((FINALBYTES / BLOCKSIZE))s" 2>/dev/null || true
truncate -s "$FINALBYTES" "$ROOTFS"

echo ""
echo "=== Golden rootfs ready at $ROOTFS ==="
ls -lh "$ROOTFS"
