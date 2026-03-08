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

echo "[1/9] Creating 6GB image..."
dd if=/dev/zero of="$ROOTFS" bs=1M count=6144 status=progress

echo "[2/9] Formatting ext4..."
mkfs.ext4 -F "$ROOTFS"

echo "[3/9] Mounting..."
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

echo "[4/9] Running debootstrap (takes ~5 minutes)..."
debootstrap --arch=amd64 \
    --include=systemd,systemd-sysv,curl,git,jq,ca-certificates,sudo,openssh-client,unzip,libicu74,iproute2,systemd-resolved \
    noble "$MNT" http://archive.ubuntu.com/ubuntu

# Fix fstab: debootstrap writes build-time loop device UUID,
# but inside Firecracker the root device is always /dev/vda.
echo -e "/dev/vda\t/\text4\tdefaults,noatime\t0\t1" > "$MNT/etc/fstab"

# Ensure /var/tmp exists (systemd-resolved needs it for PrivateTmp namespace)
mkdir -p "$MNT/var/tmp"
chmod 1777 "$MNT/var/tmp"

# Mount pseudo-filesystems needed by chroot commands
mount --bind /dev "$MNT/dev"
mount --bind /dev/pts "$MNT/dev/pts"
mount -t proc proc "$MNT/proc"
mount -t sysfs sys "$MNT/sys"

echo "[5/9] Installing packages (build tools, Docker, Rust dependencies)..."
chroot "$MNT" bash -c "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -q
    apt-get install -y --no-install-recommends \
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

echo "[6/9] Installing Rust toolchain..."
chroot "$MNT" bash -c "
    # Install rustup for the runner user
    su - runner -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable'
    # Make cargo/rustc available system-wide via symlinks
    ln -sf /home/runner/.cargo/bin/cargo /usr/local/bin/cargo
    ln -sf /home/runner/.cargo/bin/rustc /usr/local/bin/rustc
    ln -sf /home/runner/.cargo/bin/rustup /usr/local/bin/rustup
    echo 'export PATH=/home/runner/.cargo/bin:\$PATH' >> /home/runner/.bashrc
    echo 'PATH=/home/runner/.cargo/bin:/usr/local/bin:/usr/bin:/bin' > /etc/environment
"

echo "[7/9] Configuring network and runner user..."
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

# Restore systemd-resolved symlink
rm -f "$MNT/etc/resolv.conf"
ln -s /run/systemd/resolve/stub-resolv.conf "$MNT/etc/resolv.conf"

# Disable services that slow boot and aren't needed
chroot "$MNT" bash -c "
    systemctl disable apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true
    systemctl disable motd-news.timer 2>/dev/null || true
    systemctl mask systemd-timesyncd.service 2>/dev/null || true
" 2>/dev/null || true

chroot "$MNT" useradd -m -s /bin/bash runner 2>/dev/null || true
echo "runner ALL=(ALL) NOPASSWD:ALL" > "$MNT/etc/sudoers.d/runner"

echo "[8/9] Installing GitHub Actions runner v${RUNNER_VERSION}..."
curl -fsSL -o "$MNT/home/runner/actions-runner.tar.gz" \
    "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
tar xzf "$MNT/home/runner/actions-runner.tar.gz" -C "$MNT/home/runner/"
rm "$MNT/home/runner/actions-runner.tar.gz"

chroot "$MNT" /home/runner/bin/installdependencies.sh
chroot "$MNT" chown -R runner:runner /home/runner

echo "[9/9] Writing entrypoint..."
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

# Source Rust environment for CI jobs
export PATH="/home/runner/.cargo/bin:$PATH"

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

# Unmount pseudo-filesystems, then rootfs
umount "$MNT/dev/pts"
umount "$MNT/dev"
umount "$MNT/proc"
umount "$MNT/sys"
umount "$MNT"
rmdir "$MNT"
trap - EXIT

echo ""
echo "=== Golden rootfs ready at $ROOTFS ==="
ls -lh "$ROOTFS"
