#!/usr/bin/env bash
# Install mach from the latest GitHub Release into ~/.local/bin.
#   curl -fsSL https://raw.githubusercontent.com/Q1CHENL/mach/main/install.sh | sh
#
# Optional:
#   MACH_VERSION=0.1.0   # pin a tag (with or without leading v)
#   MACH_INSTALL_DIR=…   # default: $HOME/.local/bin
set -euo pipefail

REPO="Q1CHENL/mach"
BIN_NAME="mach"
INSTALL_DIR="${MACH_INSTALL_DIR:-${HOME}/.local/bin}"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "mach: need '$1' on PATH" >&2
    exit 1
  fi
}

need curl
need mktemp
need install

os="$(uname -s)"
arch="$(uname -m)"
case "${os}" in
  Linux) rust_os="unknown-linux-gnu" ;;
  Darwin) rust_os="apple-darwin" ;;
  *)
    echo "mach: unsupported OS '${os}' (Linux and macOS only)" >&2
    exit 1
    ;;
esac
case "${arch}" in
  x86_64 | amd64) rust_arch="x86_64" ;;
  arm64 | aarch64) rust_arch="aarch64" ;;
  *)
    echo "mach: unsupported arch '${arch}'" >&2
    exit 1
    ;;
esac
target="${rust_arch}-${rust_os}"
asset="${BIN_NAME}-${target}"

api_base="https://api.github.com/repos/${REPO}"
ua="mach-install"

download_url=""
if [[ -n "${MACH_VERSION:-}" ]]; then
  tag="${MACH_VERSION#v}"
  download_url="https://github.com/${REPO}/releases/download/v${tag}/${asset}"
else
  # Prefer a release that actually ships this asset (skips old Python tags).
  json="$(
    curl -fsSL \
      -H "Accept: application/vnd.github+json" \
      -H "User-Agent: ${ua}" \
      "${api_base}/releases?per_page=30"
  )"
  download_url="$(
    printf '%s' "${json}" \
      | grep -oE "https://github.com/${REPO}/releases/download/[^\"]+/${asset}" \
      | head -n1 || true
  )"
  if [[ -z "${download_url}" ]]; then
    echo "mach: no release asset '${asset}' found on GitHub" >&2
    echo "mach: publish a release, or: cargo install --git https://github.com/${REPO}" >&2
    exit 1
  fi
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "${tmpdir}"' EXIT
tmp_bin="${tmpdir}/${BIN_NAME}"

echo "Downloading ${download_url}"
if ! curl -fsSL -H "User-Agent: ${ua}" -o "${tmp_bin}" "${download_url}"; then
  echo "mach: download failed" >&2
  exit 1
fi
chmod +x "${tmp_bin}"

mkdir -p "${INSTALL_DIR}"
install -m 755 "${tmp_bin}" "${INSTALL_DIR}/${BIN_NAME}"

echo "Installed ${INSTALL_DIR}/${BIN_NAME}"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo "Note: add ${INSTALL_DIR} to your PATH, e.g.:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
echo "Run: mach"
