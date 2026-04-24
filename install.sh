#!/usr/bin/env bash

set -euo pipefail

REPO="iamkunal9/singleRPC"
BIN="singlerpc"

need() {
  command -v "$1" >/dev/null 2>&1 || { echo "[-] Required command not found: $1" >&2; exit 1; }
}

need curl
need unzip
need uname

# Detect OS
UNAME_S=$(uname -s)
case "$UNAME_S" in
  Linux)  OS="linux" ;;
  Darwin) OS="macos" ;;
  *)
    echo "[-] Unsupported OS: $UNAME_S (this installer supports Linux and macOS)" >&2
    exit 1
    ;;
esac

# The release matrix builds one binary per OS (no per-arch split).
ARCH=$(uname -m)
echo "[+] Detected: $OS ($ARCH)"

echo "[+] Fetching latest release tag..."
LATEST_TAG=$(curl -fsSL -o /dev/null -w '%{url_effective}' \
  "https://github.com/$REPO/releases/latest" \
  | sed -E 's#.*/tag/##')

if [ -z "$LATEST_TAG" ]; then
  echo "[-] Failed to fetch latest release tag" >&2
  exit 1
fi
echo "[+] Latest version: $LATEST_TAG"

FILENAME="${BIN}_${LATEST_TAG}_${OS}.zip"
DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/$FILENAME"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "[+] Downloading: $DOWNLOAD_URL"
if ! curl -fsSL -o "$TMPDIR/$FILENAME" "$DOWNLOAD_URL"; then
  echo "[-] Download failed. Asset may not exist for this OS yet." >&2
  exit 1
fi

echo "[+] Extracting..."
unzip -oq "$TMPDIR/$FILENAME" -d "$TMPDIR"

# Locate the binary inside the extracted contents.
BIN_PATH=$(find "$TMPDIR" -type f -name "$BIN" -perm -u+x 2>/dev/null | head -n1)
if [ -z "$BIN_PATH" ]; then
  BIN_PATH=$(find "$TMPDIR" -type f -name "$BIN" 2>/dev/null | head -n1)
fi
if [ -z "$BIN_PATH" ]; then
  echo "[-] Could not find '$BIN' binary inside the archive" >&2
  exit 1
fi
chmod +x "$BIN_PATH"

# Pick install dir: prefer /usr/local/bin (with sudo if needed), else ~/.local/bin.
INSTALL_DIR=""
if [ -w "/usr/local/bin" ]; then
  INSTALL_DIR="/usr/local/bin"
  SUDO=""
elif command -v sudo >/dev/null 2>&1 && [ -d "/usr/local/bin" ]; then
  INSTALL_DIR="/usr/local/bin"
  SUDO="sudo"
else
  INSTALL_DIR="$HOME/.local/bin"
  mkdir -p "$INSTALL_DIR"
  SUDO=""
fi

echo "[+] Installing to $INSTALL_DIR/$BIN..."
$SUDO install -m 0755 "$BIN_PATH" "$INSTALL_DIR/$BIN"

echo "[✓] Installed $BIN $LATEST_TAG to $INSTALL_DIR/$BIN"

# PATH hint if the chosen dir isn't on PATH.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo "[!] $INSTALL_DIR is not on your PATH."
    echo "    Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
    echo "        export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

echo "[✓] Run: $BIN --help"
