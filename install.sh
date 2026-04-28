#!/usr/bin/env bash
set -euo pipefail

REPO="uwuclxdy/clauth"
BINARY="clauth"

# If cargo is available, prefer it
if command -v cargo &>/dev/null; then
    echo "cargo detected — installing via cargo..."
    cargo install clauth
    echo ""
    echo "To uninstall, run: cargo uninstall clauth"
    exit 0
fi

# Detect OS and arch
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux)
        case "$ARCH" in
            x86_64) ASSET="clauth-linux-x86_64" ;;
            *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
        esac
        ;;
    darwin)
        case "$ARCH" in
            x86_64)        ASSET="clauth-macos-x86_64" ;;
            arm64|aarch64) ASSET="clauth-macos-aarch64" ;;
            *)             echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
        esac
        ;;
    *mingw*|*msys*|*cygwin*)
        ASSET="clauth-windows-x86_64.exe"
        BINARY="clauth.exe"
        OS="windows"
        ;;
    *)
        echo "Unsupported OS: $OS" >&2
        echo "Install via cargo: cargo install clauth"
        exit 1
        ;;
esac

URL="https://github.com/$REPO/releases/latest/download/$ASSET"
echo "Downloading $ASSET..."

TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT

if command -v curl &>/dev/null; then
    curl -fsSL "$URL" -o "$TMP"
elif command -v wget &>/dev/null; then
    wget -q "$URL" -O "$TMP"
else
    echo "Error: curl or wget is required" >&2
    exit 1
fi

chmod +x "$TMP"

# Choose install directory
if [ "$OS" = "windows" ]; then
    INSTALL_DIR="$HOME/.local/bin"
elif [ -w /usr/local/bin ]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="$HOME/.local/bin"
fi

mkdir -p "$INSTALL_DIR"
mv "$TMP" "$INSTALL_DIR/$BINARY"

echo "Installed to $INSTALL_DIR/$BINARY"

# Warn if install dir is not in PATH
if ! printf '%s' "$PATH" | grep -q "$INSTALL_DIR"; then
    echo ""
    echo "Note: $INSTALL_DIR is not in your PATH. Add this to your shell profile:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi

echo ""
echo "To uninstall, run: rm $INSTALL_DIR/$BINARY"
