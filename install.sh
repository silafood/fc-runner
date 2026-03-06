#!/usr/bin/env bash
set -euo pipefail

FC_VERSION="1.14.2"
RUNNER_VERSION="2.323.0"
ROOTFS_SIZE_MIB=4096
GUEST_IP="172.16.0.2"
HOST_IP="172.16.0.1"
TAP_DEV="tap-fc0"

echo "=== fc-runner host setup ==="

# --- 1. System dependencies ---
echo "[1/8] Installing system dependencies..."
apt-get update -qq
apt-get install -y -qq debootstrap curl jq e2fsprogs

# --- 2. Firecracker binaries ---
echo "[2/8] Installing Firecracker v${FC_VERSION}..."
ARCH=$(uname -m)
FC_URL="https://github.com/firecracker-microvm/firecracker/releases/download/v${FC_VERSION}/firecracker-v${FC_VERSION}-${ARCH}.tgz"
TMP_FC=$(mktemp -d)
curl -sL "$FC_URL" | tar xz -C "$TMP_FC"
install -m 0755 "${TMP_FC}/release-v${FC_VERSION}-${ARCH}/firecracker-v${FC_VERSION}-${ARCH}" /usr/local/bin/firecracker
install -m 0755 "${TMP_FC}/release-v${FC_VERSION}-${ARCH}/jailer-v${FC_VERSION}-${ARCH}" /usr/local/bin/jailer
rm -rf "$TMP_FC"

# --- 3. Guest kernel ---
echo "[3/8] Downloading guest kernel..."
mkdir -p /opt/fc-runner
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/${ARCH}/kernels/vmlinux.bin"
curl -sL -o /opt/fc-runner/vmlinux.bin "$KERNEL_URL"

# --- 4. Golden rootfs ---
echo "[4/8] Building golden rootfs (this takes a few minutes)..."
if [ -f /opt/fc-runner/runner-rootfs-golden.ext4 ]; then
    echo "  Golden rootfs already exists — skipping. Delete it to rebuild."
else
    ROOTFS_DIR=$(mktemp -d)
    ROOTFS_IMG="/opt/fc-runner/runner-rootfs-golden.ext4"

    # Create ext4 image
    dd if=/dev/zero of="$ROOTFS_IMG" bs=1M count=${ROOTFS_SIZE_MIB} status=progress
    mkfs.ext4 -F "$ROOTFS_IMG"

    # Mount and debootstrap
    mount -o loop "$ROOTFS_IMG" "$ROOTFS_DIR"

    debootstrap --include=systemd,systemd-sysv,curl,git,jq,ca-certificates,sudo,openssh-client \
        noble "$ROOTFS_DIR" http://archive.ubuntu.com/ubuntu

    # Configure networking inside the rootfs
    cat > "${ROOTFS_DIR}/etc/systemd/network/eth0.network" <<NETEOF
[Match]
Name=eth0

[Network]
Address=${GUEST_IP}/24
Gateway=${HOST_IP}
DNS=8.8.8.8
NETEOF

    # Enable systemd-networkd
    chroot "$ROOTFS_DIR" systemctl enable systemd-networkd systemd-resolved

    # Install GitHub Actions runner
    mkdir -p "${ROOTFS_DIR}/home/runner/actions-runner"
    RUNNER_URL="https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
    curl -sL "$RUNNER_URL" | tar xz -C "${ROOTFS_DIR}/home/runner/actions-runner"

    # Create runner user
    chroot "$ROOTFS_DIR" useradd -d /home/runner -s /bin/bash runner || true
    chroot "$ROOTFS_DIR" chown -R runner:runner /home/runner

    # Write entrypoint script
    cat > "${ROOTFS_DIR}/entrypoint.sh" <<'ENTRY'
#!/bin/bash
set -e
source /run/fc-runner-env

cd /home/runner/actions-runner
./config.sh \
    --url "$REPO_URL" \
    --token "$RUNNER_TOKEN" \
    --unattended \
    --ephemeral \
    --name "fc-$VM_ID" \
    --labels self-hosted,linux,firecracker \
    --work _work

sudo -u runner ./run.sh
reboot -f
ENTRY
    chmod +x "${ROOTFS_DIR}/entrypoint.sh"

    # Set entrypoint to run on boot via rc.local
    cat > "${ROOTFS_DIR}/etc/rc.local" <<'RCEOF'
#!/bin/bash
/entrypoint.sh &
RCEOF
    chmod +x "${ROOTFS_DIR}/etc/rc.local"

    # Set root password (for debugging only)
    chroot "$ROOTFS_DIR" bash -c 'echo "root:fc-runner" | chpasswd'

    # Unmount
    umount "$ROOTFS_DIR"
    rmdir "$ROOTFS_DIR"
    echo "  Golden rootfs created: $ROOTFS_IMG"
fi

# --- 5. TAP interface + NAT ---
echo "[5/8] Setting up TAP interface and NAT..."
if ip link show "$TAP_DEV" &>/dev/null; then
    echo "  TAP device $TAP_DEV already exists — skipping."
else
    ip tuntap add "$TAP_DEV" mode tap
    ip addr add "${HOST_IP}/24" dev "$TAP_DEV"
    ip link set "$TAP_DEV" up
fi

# Enable IP forwarding
sysctl -w net.ipv4.ip_forward=1 > /dev/null

# NAT rules (idempotent — check before adding)
DEFAULT_IFACE=$(ip route | awk '/default/ {print $5; exit}')
if ! iptables -t nat -C POSTROUTING -o "$DEFAULT_IFACE" -j MASQUERADE 2>/dev/null; then
    iptables -t nat -A POSTROUTING -o "$DEFAULT_IFACE" -j MASQUERADE
fi
if ! iptables -C FORWARD -i "$TAP_DEV" -o "$DEFAULT_IFACE" -j ACCEPT 2>/dev/null; then
    iptables -A FORWARD -i "$TAP_DEV" -o "$DEFAULT_IFACE" -j ACCEPT
fi
if ! iptables -C FORWARD -i "$DEFAULT_IFACE" -o "$TAP_DEV" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null; then
    iptables -A FORWARD -i "$DEFAULT_IFACE" -o "$TAP_DEV" -m state --state RELATED,ESTABLISHED -j ACCEPT
fi

# --- 6. Directories ---
echo "[6/8] Creating directories..."
mkdir -p /etc/fc-runner
mkdir -p /var/lib/fc-runner/vms

# --- 7. Config files ---
echo "[7/8] Installing config files..."
if [ ! -f /etc/fc-runner/config.toml ]; then
    cp config.toml.example /etc/fc-runner/config.toml
    echo "  Copied config.toml.example -> /etc/fc-runner/config.toml (edit with your token!)"
else
    echo "  /etc/fc-runner/config.toml already exists — not overwriting."
fi
cp vm-config.json.template /etc/fc-runner/vm-config.json.template

# --- 8. Systemd service ---
echo "[8/8] Installing systemd service..."
cp fc-runner.service /etc/systemd/system/fc-runner.service
systemctl daemon-reload
systemctl enable fc-runner

echo ""
echo "=== Setup complete ==="
echo "Next steps:"
echo "  1. Edit /etc/fc-runner/config.toml with your GitHub PAT and repo"
echo "  2. cargo build --release && sudo install -m 0755 target/release/fc-runner /usr/local/bin/fc-runner"
echo "  3. sudo systemctl start fc-runner"
echo "  4. sudo journalctl -u fc-runner -f"
