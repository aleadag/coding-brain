#!/bin/sh
# Coding Brain installer — downloads the latest release binary for your platform.
# Usage: curl -fsSL https://raw.githubusercontent.com/aleadag/coding-brain/main/install.sh | sh

set -e

REPO="aleadag/coding-brain"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

if command -v shasum >/dev/null 2>&1; then
    verify_checksum() {
    shasum -a 256 -c "$1"
    }
elif command -v sha256sum >/dev/null 2>&1; then
    verify_checksum() {
    sha256sum -c "$1"
    }
else
    echo "Error: checksum verification requires shasum or sha256sum" >&2
    exit 1
fi

# Detect OS and architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin)  OS_TARGET="apple-darwin" ;;
    Linux)   OS_TARGET="unknown-linux-musl" ;;
    *)       echo "Error: unsupported OS: $OS" >&2; exit 1 ;;
esac

case "$ARCH" in
    x86_64|amd64)   ARCH_TARGET="x86_64" ;;
    aarch64|arm64)   ARCH_TARGET="aarch64" ;;
    *)               echo "Error: unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

TARGET="${ARCH_TARGET}-${OS_TARGET}"

# Get latest release tag
echo "Fetching latest release..."
LATEST=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

if [ -z "$LATEST" ]; then
    echo "Error: could not determine latest release" >&2
    exit 1
fi

echo "Installing Coding Brain ${LATEST} for ${TARGET}..."

ARCHIVE="coding-brain-${LATEST}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${LATEST}/${ARCHIVE}"
CHECKSUM_URL="${URL}.sha256"

# Download to temp directory
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL -o "${TMP_DIR}/checksum.sha256" "$CHECKSUM_URL"
curl -fsSL -o "${TMP_DIR}/${ARCHIVE}" "$URL"

(
    cd "$TMP_DIR"
    verify_checksum checksum.sha256
)

# Extract and install
tar xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR"

if [ -w "$INSTALL_DIR" ]; then
    install -m 0755 "${TMP_DIR}/coding-brain" "${INSTALL_DIR}/coding-brain"
else
    echo "Installing to ${INSTALL_DIR} (requires sudo)..."
    sudo install -m 0755 "${TMP_DIR}/coding-brain" "${INSTALL_DIR}/coding-brain"
fi

echo "Coding Brain ${LATEST} installed to ${INSTALL_DIR}/coding-brain"
echo "Run 'coding-brain init' to get started."
