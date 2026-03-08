#!/usr/bin/env bash
set -euo pipefail

FC_VERSION="1.14.2"

echo "=== fc-runner host setup ==="
echo "NOTE: Network setup (TAP, NAT, IP forwarding) is handled by fc-runner at startup."

# --- 1. System dependencies ---
echo "[1/5] Installing system dependencies..."
apt-get update -qq
apt-get install -y -qq debootstrap curl jq e2fsprogs unzip wget iptables ipset iproute2

# --- 2. Firecracker binaries ---
echo "[2/5] Installing Firecracker v${FC_VERSION}..."
ARCH=$(uname -m)
FC_URL="https://github.com/firecracker-microvm/firecracker/releases/download/v${FC_VERSION}/firecracker-v${FC_VERSION}-${ARCH}.tgz"
TMP_FC=$(mktemp -d)
curl -sL "$FC_URL" | tar xz -C "$TMP_FC"
install -m 0755 "${TMP_FC}/release-v${FC_VERSION}-${ARCH}/firecracker-v${FC_VERSION}-${ARCH}" /usr/local/bin/firecracker
install -m 0755 "${TMP_FC}/release-v${FC_VERSION}-${ARCH}/jailer-v${FC_VERSION}-${ARCH}" /usr/local/bin/jailer
rm -rf "$TMP_FC"

# --- 3. Directories ---
echo "[3/5] Creating directories..."
mkdir -p /etc/fc-runner
mkdir -p /var/lib/fc-runner/vms
mkdir -p /opt/fc-runner

# --- 4. Config files ---
echo "[4/5] Installing config files..."
if [ ! -f /etc/fc-runner/config.toml ]; then
    cp config.toml.example /etc/fc-runner/config.toml
    chmod 0600 /etc/fc-runner/config.toml
    echo "  Copied config.toml.example -> /etc/fc-runner/config.toml (edit with your token!)"
else
    echo "  /etc/fc-runner/config.toml already exists — not overwriting."
fi
# Ensure config is not world-readable (fc-runner refuses to start otherwise)
chown root:root /etc/fc-runner/config.toml
chmod 0600 /etc/fc-runner/config.toml
cp vm-config.json.template /etc/fc-runner/vm-config.json.template

# --- 5. Systemd service ---
echo "[5/5] Installing systemd service..."
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
