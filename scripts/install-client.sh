#!/usr/bin/env bash
# ProxyGit Client Universal Installer (macOS & Linux)
set -euo pipefail

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
BIN_URL="https://github.com/proxygit/releases/latest/download/proxygit-client-${OS}-${ARCH}.tar.gz"

echo "==> ProxyGit Client v0.1.0 Installer"
echo "==> Detected: $OS / $ARCH"
echo ""

# Install platform dependency
if [ "$OS" = "darwin" ]; then
    if ! command -v brew &>/dev/null; then
        echo "Error: Homebrew is required on macOS." && exit 1
    fi
    echo "==> Verifying macFUSE dependency..."
    brew list --cask macfuse &>/dev/null || brew install --cask macfuse
elif [ "$OS" = "linux" ]; then
    echo "==> Verifying FUSE3 dependency..."
    if command -v apt-get &>/dev/null; then
        sudo apt-get update && sudo apt-get install -y fuse3 libfuse3-dev
    elif command -v dnf &>/dev/null; then
        sudo dnf install -y fuse3 fuse3-devel
    else
        echo "Warning: Please install FUSE3 manually for your distribution."
    fi
fi

# Create directories
INSTALL_DIR="/usr/local/bin"
mkdir -p "$HOME/ProxyGit" "$HOME/.config/proxygit"

# Write default config
cat <<EOF > "$HOME/.config/proxygit/config.toml"
server_addr = "127.0.0.1:8080"
mount_point = "$HOME/ProxyGit"
cache_dir = "$HOME/.cache/proxygit/cache"
wal_dir = "$HOME/.cache/proxygit/wal"
build_cache_dir = "$HOME/.cache/proxygit/build_cache"
EOF

echo ""
echo "==> Config written to $HOME/.config/proxygit/config.toml"
echo "==> To start: proxygit-client mount <server_addr> <project_id>"
echo "==> Edit your server address in the config file."
echo ""
echo "ProxyGit installation complete!"
