#!/bin/sh
# Knaix CLI installer.
#
#   curl -sSL https://knaix.com/install.sh | sh
#
# Downloads the correct prebuilt binary for your OS/arch from the latest
# GitHub Release, verifies its checksum, and installs it.
#
# Overrides (environment variables):
#   KNAIX_VERSION       tag to install (default: latest), e.g. v0.3.4
#   KNAIX_INSTALL_DIR   install directory (default: /usr/local/bin)

set -eu

REPO="kovalentai/knaix-cli"
BIN="knaix"
VERSION="${KNAIX_VERSION:-latest}"
INSTALL_DIR="${KNAIX_INSTALL_DIR:-/usr/local/bin}"

err() { echo "Error: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

have curl || err "curl is required."
have tar || err "tar is required."

# --- Detect platform ---------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  os_id="unknown-linux-musl" ;;
  Darwin) os_id="apple-darwin" ;;
  *) err "Unsupported operating system: $os" ;;
esac
case "$arch" in
  x86_64|amd64)   arch_id="x86_64" ;;
  arm64|aarch64)  arch_id="aarch64" ;;
  *) err "Unsupported architecture: $arch" ;;
esac
target="${arch_id}-${os_id}"

# --- Resolve version ---------------------------------------------------------
if [ "$VERSION" = "latest" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -n1 | cut -d '"' -f4)"
  [ -n "$VERSION" ] || err "Could not determine the latest release."
fi

asset="${BIN}-${VERSION}-${target}.tar.gz"
base="https://github.com/${REPO}/releases/download/${VERSION}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

# --- Download ----------------------------------------------------------------
echo "Downloading ${asset} ..."
curl -fsSL "${base}/${asset}"    -o "${tmp}/${asset}"   || err "Download failed for ${asset}."
curl -fsSL "${base}/SHA256SUMS"  -o "${tmp}/SHA256SUMS" || err "Download failed for SHA256SUMS."

# --- Verify checksum ---------------------------------------------------------
echo "Verifying checksum ..."
expected="$(grep " ${asset}\$" "${tmp}/SHA256SUMS" | awk '{print $1}')"
[ -n "$expected" ] || err "No checksum listed for ${asset}."
if have sha256sum; then
  actual="$(sha256sum "${tmp}/${asset}" | awk '{print $1}')"
elif have shasum; then
  actual="$(shasum -a 256 "${tmp}/${asset}" | awk '{print $1}')"
else
  err "Need sha256sum or shasum to verify the download."
fi
[ "$expected" = "$actual" ] || err "Checksum mismatch for ${asset} (expected ${expected}, got ${actual})."

# --- Install -----------------------------------------------------------------
tar -xzf "${tmp}/${asset}" -C "$tmp"
[ -f "${tmp}/${BIN}" ] || err "Archive did not contain the ${BIN} binary."
chmod +x "${tmp}/${BIN}"

install_to() { mv "${tmp}/${BIN}" "${1}/${BIN}"; }
if mkdir -p "$INSTALL_DIR" 2>/dev/null && [ -w "$INSTALL_DIR" ]; then
  install_to "$INSTALL_DIR"
elif have sudo; then
  echo "Writing to ${INSTALL_DIR} requires elevated permissions ..."
  sudo mkdir -p "$INSTALL_DIR"
  sudo mv "${tmp}/${BIN}" "${INSTALL_DIR}/${BIN}"
else
  err "Cannot write to ${INSTALL_DIR}. Re-run with KNAIX_INSTALL_DIR set to a writable path."
fi

echo "Installed ${BIN} ${VERSION} to ${INSTALL_DIR}/${BIN}"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Note: ${INSTALL_DIR} is not on your PATH; add it to use '${BIN}' directly." ;;
esac

cat <<'EOF'

========================================================
   Kovalent (Knaix) CLI installed
========================================================

Quick start (no account needed, just Docker):

  1. Stand up a private AI node and pick a model (or the mock):
     $ knaix local setup

  2. Give it some documents:
     $ knaix upload ./README.md

  3. Ask a question grounded in them:
     $ knaix chat "what does this cover?"

Step 1 makes 'local' your default node, so steps 2 and 3 need no '-n local'.
Clear the node's store any time and start fresh with 'knaix local reset'.

Prefer a hosted node? Run 'knaix login', then 'knaix up'.

For more commands, run: knaix --help
Documentation: https://knaix.com
EOF
