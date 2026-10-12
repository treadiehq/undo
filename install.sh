#!/bin/sh
set -e

REPO="treadiehq/undo"
BINARY="undo"

# Detect platform
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Darwin) os="apple-darwin" ;;
  Linux)  os="unknown-linux-gnu" ;;
  *)
    echo "Error: unsupported OS: $OS"
    exit 1
    ;;
esac

case "$ARCH" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64)  arch="x86_64" ;;
  *)
    echo "Error: unsupported architecture: $ARCH"
    exit 1
    ;;
esac

TARGET="${arch}-${os}"

# Get latest release tag
echo "Fetching latest release..."
LATEST=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')

if [ -z "$LATEST" ]; then
  echo "Error: could not determine latest release."
  exit 1
fi

URL="https://github.com/${REPO}/releases/download/${LATEST}/${BINARY}-${LATEST}-${TARGET}.tar.gz"

echo "Downloading ${BINARY} ${LATEST} for ${TARGET}..."

UNDO_INSTALL_TMP=$(mktemp -d)
trap 'rm -rf "$UNDO_INSTALL_TMP"' EXIT

curl -fsSL "$URL" -o "${UNDO_INSTALL_TMP}/${BINARY}.tar.gz"
tar xzf "${UNDO_INSTALL_TMP}/${BINARY}.tar.gz" -C "$UNDO_INSTALL_TMP"

# Check the staged executable before replacing a working installation. In
# particular, a missing glibc version must fail the installer, not just print
# an error hidden by a successful `head` process at the end of a pipeline.
chmod +x "${UNDO_INSTALL_TMP}/${BINARY}"
if ! VERSION=$("${UNDO_INSTALL_TMP}/${BINARY}" --version); then
  echo "Error: the downloaded ${BINARY} cannot run on this system; installation cancelled." >&2
  exit 1
fi

# Install to /usr/local/bin if writable, otherwise ~/.local/bin
INSTALL_DIR="/usr/local/bin"
if [ ! -w "$INSTALL_DIR" ]; then
  INSTALL_DIR="${HOME}/.local/bin"
  mkdir -p "$INSTALL_DIR"
fi

mv "${UNDO_INSTALL_TMP}/${BINARY}" "${INSTALL_DIR}/${BINARY}"

echo ""
echo "Installed ${BINARY} ${LATEST} to ${INSTALL_DIR}/${BINARY}"

if [ "$INSTALL_DIR" = "${HOME}/.local/bin" ]; then
  case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *) echo "Add ${INSTALL_DIR} to your PATH: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
  esac
fi

echo ""
echo "$VERSION"
echo ""
echo "Run 'undo start' in any project directory to begin."
