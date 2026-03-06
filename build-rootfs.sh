#!/usr/bin/env bash
set -euo pipefail

ROOTFS="/opt/fc-runner/runner-rootfs-golden.ext4"
MNT="/opt/fc-runner/rootfs-build"
RUNNER_VERSION="2.323.0"

echo "=== Building golden rootfs ==="

# Clean up any previous failed build
umount -l "$MNT" 2>/dev/null || true
rm -rf "$MNT"
rm -f "$ROOTFS"

echo "[1/7] Creating 4GB image..."
dd if=/dev/zero of="$ROOTFS" bs=1M count=4096 status=progress

echo "[2/7] Formatting ext4..."
mkfs.ext4 -F "$ROOTFS"

echo "[3/7] Mounting..."
mkdir -p "$MNT"
mount -o loop "$ROOTFS" "$MNT"

# Ensure cleanup on exit
cleanup() {
    echo "Cleaning up mount..."
    umount -l "$MNT" 2>/dev/null || true
    rmdir "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

echo "[4/7] Running debootstrap (takes ~5 minutes)..."
debootstrap --arch=amd64 \
    --include=systemd,systemd-sysv,curl,git,jq,ca-certificates,sudo,openssh-client,unzip,libicu74 \
    noble "$MNT" http://archive.ubuntu.com/ubuntu

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

chroot "$MNT" /home/runner/bin/installdependencies.sh
chroot "$MNT" chown -R runner:runner /home/runner

echo "[7/7] Writing entrypoint..."
cat > "$MNT/entrypoint.sh" << 'ENTRYEOF'
#!/bin/bash
set -euo pipefail
exec > /var/log/runner.log 2>&1

echo "=== fc-runner entrypoint ==="
date

if [ ! -f /etc/fc-runner-env ]; then
    echo "ERROR: /etc/fc-runner-env not found!"
    ls -la /etc/fc-runner* 2>/dev/null || echo "No fc-runner files in /etc/"
    sleep 5
    reboot -f
fi

source /etc/fc-runner-env
echo "REPO_URL=${REPO_URL}"
echo "VM_ID=${VM_ID}"

cd /home/runner
echo "Configuring runner..."
sudo -u runner ./config.sh \
    --url "${REPO_URL}" \
    --token "${RUNNER_TOKEN}" \
    --labels "firecracker,linux,x64" \
    --ephemeral \
    --unattended \
    --work /home/runner/_work

echo "Starting runner..."
sudo -u runner ./run.sh

echo "Runner finished, shutting down..."
reboot -f
ENTRYEOF
chmod +x "$MNT/entrypoint.sh"

cat > "$MNT/etc/rc.local" << 'RCEOF'
#!/bin/bash
/entrypoint.sh &
exit 0
RCEOF
chmod +x "$MNT/etc/rc.local"

# Unmount (trap will handle cleanup)
umount "$MNT"
rmdir "$MNT"
trap - EXIT

echo ""
echo "=== Golden rootfs ready at $ROOTFS ==="
ls -lh "$ROOTFS"
