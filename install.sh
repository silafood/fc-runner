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
apt-get install -y -qq debootstrap curl jq e2fsprogs unzip wget

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
if [ -f /opt/fc-runner/vmlinux.bin ]; then
    echo "  Kernel already exists — skipping."
else
    KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/${ARCH}/kernels/vmlinux.bin"
    wget -O /opt/fc-runner/vmlinux.bin "$KERNEL_URL"
fi

# --- 4. Golden rootfs ---
echo "[4/8] Building golden rootfs (this takes a few minutes)..."
if [ -f /opt/fc-runner/runner-rootfs-golden.ext4 ]; then
    echo "  Golden rootfs already exists — skipping. Delete it to rebuild."
else
    ROOTFS_IMG="/opt/fc-runner/runner-rootfs-golden.ext4"

    # Create ext4 image
    dd if=/dev/zero of="$ROOTFS_IMG" bs=1M count=${ROOTFS_SIZE_MIB} status=progress
    mkfs.ext4 -F "$ROOTFS_IMG"

    # Bootstrap into temp dir then copy into image
    sudo debootstrap --arch=amd64 noble /tmp/fc-rootfs

    # Configure inside chroot
    sudo chroot /tmp/fc-rootfs /bin/bash <<'CHROOT'
# Minimal dependencies for GitHub runner
apt update && apt install -y --no-install-recommends \
  curl git jq unzip ca-certificates sudo \
  libicu74 liblttng-ust1

# Create runner user
useradd -m -s /bin/bash runner
echo "runner ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers.d/runner

# Install GitHub Actions runner v2.323.0
cd /home/runner
curl -fsSL -o runner.tar.gz \
  https://github.com/actions/runner/releases/download/v2.323.0/actions-runner-linux-x64-2.323.0.tar.gz
tar xzf runner.tar.gz && rm runner.tar.gz
./bin/installdependencies.sh
chown -R runner:runner /home/runner

# Entrypoint: register (JIT) → run one job → auto-deregister
cat > /entrypoint.sh <<'EOF'
#!/bin/bash
set -euo pipefail
# Token injected at VM boot via kernel cmdline or vsock
source /run/fc-runner-env

cd /home/runner
sudo -u runner ./config.sh \
  --url "${REPO_URL}" \
  --token "${RUNNER_TOKEN}" \
  --name "fc-$(cat /proc/sys/kernel/hostname)" \
  --labels "firecracker,linux,x64" \
  --ephemeral \
  --unattended \
  --work /home/runner/_work

sudo -u runner ./run.sh
EOF
chmod +x /entrypoint.sh

# Set entrypoint in rc.local
cat > /etc/rc.local <<'EOF'
#!/bin/bash
/entrypoint.sh >> /var/log/runner.log 2>&1 &
exit 0
EOF
chmod +x /etc/rc.local

# Network config (virtio-net)
cat > /etc/systemd/network/20-eth.network <<'EOF'
[Match]
Name=eth0

[Network]
Address=172.16.0.2/24
Gateway=172.16.0.1
DNS=8.8.8.8
EOF

systemctl enable systemd-networkd
CHROOT

    # Package rootfs into ext4 image
    mount -o loop "$ROOTFS_IMG" /mnt
    cp -a /tmp/fc-rootfs/. /mnt/
    umount /mnt

    # Cleanup temp rootfs
    rm -rf /tmp/fc-rootfs
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
