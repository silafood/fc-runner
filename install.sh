#!/usr/bin/env bash
set -euo pipefail

FC_VERSION="1.14.2"

echo "=== fc-runner host setup ==="
echo "NOTE: Network setup (TAP, NAT, IP forwarding) is handled by fc-runner at startup."

# --- 1. System dependencies ---
echo "[1/5] Installing system dependencies..."
apt-get update -qq
apt-get install -y -qq debootstrap curl jq e2fsprogs unzip wget iptables ipset iproute2 apparmor-utils

# --- 2. Firecracker binaries ---
echo "[2/5] Installing Firecracker v${FC_VERSION}..."
ARCH=$(uname -m)
FC_URL="https://github.com/firecracker-microvm/firecracker/releases/download/v${FC_VERSION}/firecracker-v${FC_VERSION}-${ARCH}.tgz"
TMP_FC=$(mktemp -d)
curl -sL "$FC_URL" | tar xz -C "$TMP_FC"
install -m 0755 "${TMP_FC}/release-v${FC_VERSION}-${ARCH}/firecracker-v${FC_VERSION}-${ARCH}" /usr/local/bin/firecracker
install -m 0755 "${TMP_FC}/release-v${FC_VERSION}-${ARCH}/jailer-v${FC_VERSION}-${ARCH}" /usr/local/bin/jailer
rm -rf "$TMP_FC"

# --- 3. AppArmor profiles ---
echo "[3/6] Installing AppArmor profiles..."
if [ -d apparmor ]; then
    cp apparmor/usr.local.bin.firecracker /etc/apparmor.d/usr.local.bin.firecracker
    cp apparmor/usr.local.bin.fc-runner /etc/apparmor.d/usr.local.bin.fc-runner
    apparmor_parser -r -W /etc/apparmor.d/usr.local.bin.firecracker 2>/dev/null || true
    apparmor_parser -r -W /etc/apparmor.d/usr.local.bin.fc-runner 2>/dev/null || true
    echo "  AppArmor profiles installed. fc-runner will enforce them at startup."
else
    echo "  apparmor/ directory not found — skipping profile install."
fi

# --- 4. Directories ---
echo "[4/6] Creating directories..."
mkdir -p /etc/fc-runner
mkdir -p /var/lib/fc-runner/vms
mkdir -p /opt/fc-runner

# --- 5. Config files ---
echo "[5/6] Installing config files..."
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

# --- 6. Systemd service ---
echo "[6/6] Installing systemd service..."
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
