#!/bin/sh
# Install a prebuilt mcp-lazy binary. No Rust toolchain needed.
#
# Detects your OS and architecture, downloads the matching archive from the
# latest GitHub release, verifies its SHA-256 checksum, and installs the
# binary to ~/.local/bin (set MCP_LAZY_INSTALL_DIR to change it).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ericmason/mcp-lazy/master/scripts/install.sh | sh
#
#   # pin a version or change the target directory:
#   curl -fsSL .../install.sh | MCP_LAZY_VERSION=v0.1.0 MCP_LAZY_INSTALL_DIR=/usr/local/bin sh

set -eu

REPO="ericmason/mcp-lazy"
BINARY="mcp-lazy"
INSTALL_DIR="${MCP_LAZY_INSTALL_DIR:-${HOME}/.local/bin}"

die() { echo "error: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

fetch() { # URL -> stdout
    if have curl; then curl -fsSL "$1"
    elif have wget; then wget -qO- "$1"
    else die "need curl or wget on PATH"; fi
}
download() { # URL FILE
    if have curl; then curl -fsSL "$1" -o "$2"
    elif have wget; then wget -qO "$2" "$1"
    else die "need curl or wget on PATH"; fi
}
sha256() { # FILE -> hex digest, or empty if no tool
    if have sha256sum; then sha256sum "$1" | awk '{print $1}'
    elif have shasum; then shasum -a 256 "$1" | awk '{print $1}'
    else echo ""; fi
}

os=$(uname -s)
case "$os" in
    Darwin) os=darwin ;;
    Linux)  os=linux ;;
    *) die "unsupported OS '$os'; mcp-lazy runs on macOS and Linux" ;;
esac

arch=$(uname -m)
case "$arch" in
    x86_64|amd64)  arch=amd64 ;;
    arm64|aarch64) arch=arm64 ;;
    *) die "unsupported architecture '$arch'" ;;
esac

version="${MCP_LAZY_VERSION:-}"
if [ -z "$version" ]; then
    version=$(fetch "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' | head -n1 \
        | sed -E 's/.*"tag_name" *: *"([^"]+)".*/\1/')
fi
[ -n "$version" ] || die "could not find the latest version (set MCP_LAZY_VERSION=vX.Y.Z)"
version="v${version#v}"

asset="${BINARY}-${version}-${os}-${arch}.tar.gz"
base="https://github.com/${REPO}/releases/download/${version}"

echo "Installing ${BINARY} ${version} (${os}/${arch})..."

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

download "${base}/${asset}" "${tmp}/${asset}" \
    || die "download failed: ${base}/${asset}"
download "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" \
    || die "could not download SHA256SUMS"

expected=$(awk -v f="$asset" '{name=$2; sub(/^\*/,"",name); if (name==f) print $1}' "${tmp}/SHA256SUMS")
actual=$(sha256 "${tmp}/${asset}")
[ -n "$expected" ] || die "no checksum for ${asset} in SHA256SUMS"
[ -n "$actual" ] || die "need sha256sum or shasum to verify the download"
[ "$expected" = "$actual" ] || die "checksum mismatch for ${asset}
  expected: ${expected}
  actual:   ${actual}"
echo "Checksum verified."

tar -xzf "${tmp}/${asset}" -C "${tmp}" || die "could not extract ${asset}"
src="${tmp}/${asset%.tar.gz}/${BINARY}"
[ -f "$src" ] || die "${BINARY} not found in the archive"

mkdir -p "$INSTALL_DIR"
if ! install -m 0755 "$src" "${INSTALL_DIR}/${BINARY}" 2>/dev/null; then
    { cp "$src" "${INSTALL_DIR}/${BINARY}" && chmod 0755 "${INSTALL_DIR}/${BINARY}"; } \
        || die "could not install to ${INSTALL_DIR} (set MCP_LAZY_INSTALL_DIR to a folder you can write)"
fi

echo "Installed ${INSTALL_DIR}/${BINARY}"
"${INSTALL_DIR}/${BINARY}" --version 2>/dev/null || true
echo "Use this full path as the command in your MCP configs."

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *) echo
       echo "note: ${INSTALL_DIR} is not on your PATH. To run mcp-lazy by name, add this to your shell rc:"
       echo "  export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac
