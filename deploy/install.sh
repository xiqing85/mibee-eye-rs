#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# MiBee Eye RaspPi — Install Script
# ============================================================================
# Usage:
#   ./deploy/install.sh <user@host>
#
# Example:
#   ./deploy/install.sh pi@192.168.1.100
#
# Cross-compile first:
#   make cross-build && ./deploy/install.sh pi@192.168.1.100
# ============================================================================

if [ $# -lt 1 ]; then
    echo "Usage: $0 <user@host>"
    echo "Example: $0 pi@192.168.1.100"
    exit 1
fi

REMOTE_HOST="$1"
INSTALL_DIR="/tmp/mibee-eye-install"
BINARY="target/aarch64-unknown-linux-gnu/release/mibee-eye-raspi-rs"
CONFIG="config.example.toml"
SERVICE="deploy/mibee-eye-raspi-rs.service"

echo "==> MiBee Eye Installer — target: $REMOTE_HOST"

# --- Check artifacts ---
if [ ! -f "$BINARY" ]; then
    echo "ERROR: Binary not found at $BINARY"
    echo "Build it first: make cross-build"
    exit 1
fi

if [ ! -f "$SERVICE" ]; then
    echo "ERROR: Service file not found at $SERVICE"
    exit 1
fi

# --- Stage files on remote ---
echo "==> Staging files on $REMOTE_HOST..."
ssh "$REMOTE_HOST" "mkdir -p $INSTALL_DIR"
scp "$BINARY" "$REMOTE_HOST:$INSTALL_DIR/mibee-eye-raspi-rs"
scp "$CONFIG" "$REMOTE_HOST:$INSTALL_DIR/config.toml"
scp "$SERVICE" "$REMOTE_HOST:$INSTALL_DIR/mibee-eye-raspi-rs.service"

# --- Install on remote ---
echo "==> Installing on $REMOTE_HOST..."
ssh "$REMOTE_HOST" bash -s <<'REMOTE_SCRIPT'
set -euo pipefail

INSTALL_DIR="/tmp/mibee-eye-install"

# Create mibee user if not exists
if ! id mibee &>/dev/null; then
    sudo useradd --system --no-create-home --shell /usr/sbin/nologin mibee
    echo "  -> Created system user 'mibee'"
fi

# Create directories
sudo mkdir -p /usr/local/bin
sudo mkdir -p /etc/mibee-eye
sudo mkdir -p /var/lib/mibee-eye

# Install binary
sudo cp "$INSTALL_DIR/mibee-eye-raspi-rs" /usr/local/bin/mibee-eye-raspi-rs
sudo chmod 755 /usr/local/bin/mibee-eye-raspi-rs

# Install config (don't overwrite existing)
if [ ! -f /etc/mibee-eye/config.toml ]; then
    sudo cp "$INSTALL_DIR/config.toml" /etc/mibee-eye/config.toml
    sudo chmod 640 /etc/mibee-eye/config.toml
    echo "  -> Created default config at /etc/mibee-eye/config.toml"
    echo "  -> EDIT /etc/mibee-eye/config.toml with your camera settings"
else
    echo "  -> Config already exists at /etc/mibee-eye/config.toml (skipped)"
fi

# Set ownership
sudo chown -R mibee:mibee /etc/mibee-eye
sudo chown -R mibee:mibee /var/lib/mibee-eye

# Install systemd service
sudo cp "$INSTALL_DIR/mibee-eye-raspi-rs.service" /etc/systemd/system/mibee-eye-raspi-rs.service
sudo chmod 644 /etc/systemd/system/mibee-eye-raspi-rs.service

# Reload and enable
sudo systemctl daemon-reload
sudo systemctl enable mibee-eye-raspi-rs.service

echo "==> Service installed and enabled."
echo "==> Configure /etc/mibee-eye/config.toml then:"
echo "    sudo systemctl start mibee-eye-raspi-rs"

# Clean up
rm -rf "$INSTALL_DIR"
REMOTE_SCRIPT

echo "==> Done! MiBee Eye installed on $REMOTE_HOST."
