#!/bin/sh
# Install one verified mach GitHub release into ~/.local/bin.
#
# Optional:
#   MACH_VERSION=v0.2.0       exact stable tag (otherwise GitHub's latest stable)
#   MACH_INSTALL_DIR=/path    default: $HOME/.local/bin
set -eu

REPO=Q1CHENL/mach
BIN_NAME=mach
CHECKSUMS_ASSET=SHA256SUMS
INSTALL_DIR=${MACH_INSTALL_DIR:-${HOME}/.local/bin}
RELEASE_BASE=${MACH_RELEASE_BASE_URL:-https://github.com/${REPO}/releases/download}
API_LATEST=${MACH_API_LATEST_URL:-https://api.github.com/repos/${REPO}/releases/latest}
USER_AGENT=mach-install
MAX_TEXT_BYTES=1048576
MAX_BINARY_BYTES=134217728

fail() {
  printf 'mach: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "need '$1' on PATH"
}

version_at_least() {
  actual=$1
  required_major=$2
  required_minor=$3
  actual_major=${actual%%.*}
  actual_rest=${actual#*.}
  actual_minor=${actual_rest%%.*}
  case "${actual_major}:${actual_minor}" in
    *[!0-9:]* | :*) return 1 ;;
  esac
  [ "$actual_major" -gt "$required_major" ] || {
    [ "$actual_major" -eq "$required_major" ] \
      && [ "$actual_minor" -ge "$required_minor" ]
  }
}

need awk
need chmod
need curl
need grep
need mkdir
need mktemp
need mv
need sed
need tr
need uname
need wc

download() {
  download_url=$1
  download_path=$2
  download_limit=$3
  download_label=$4

  curl -fsSL --retry 3 --max-filesize "$download_limit" \
    -H "User-Agent: ${USER_AGENT}" \
    -o "$download_path" "$download_url" \
    || fail "could not download $download_label"

  download_size=$(wc -c < "$download_path" | tr -d '[:space:]')
  case "$download_size" in
    '' | *[!0-9]*) fail "could not measure $download_label" ;;
  esac
  [ "$download_size" -le "$download_limit" ] \
    || fail "$download_label exceeds the ${download_limit}-byte limit"
}

os=$(uname -s)
arch=$(uname -m)
case "$arch" in
  x86_64 | amd64) rust_arch=x86_64 ;;
  arm64 | aarch64) rust_arch=aarch64 ;;
  *) fail "unsupported architecture '$arch'" ;;
esac

case "$os" in
  Darwin)
    need sw_vers
    macos_version=$(sw_vers -productVersion)
    if [ "$rust_arch" = aarch64 ]; then
      version_at_least "$macos_version" 11 0 \
        || fail "macOS 11.0 or newer is required on Apple Silicon"
    else
      version_at_least "$macos_version" 10 12 \
        || fail "macOS 10.12 or newer is required on Intel"
    fi
    target=${rust_arch}-apple-darwin
    ;;
  Linux)
    glibc_info=
    if command -v getconf >/dev/null 2>&1; then
      glibc_info=$(getconf GNU_LIBC_VERSION 2>/dev/null || :)
    fi
    case "$glibc_info" in
      'glibc '*) glibc_version=${glibc_info#glibc } ;;
      *) fail "unsupported Linux libc; mach release binaries require glibc 2.28 or newer" ;;
    esac
    version_at_least "$glibc_version" 2 28 \
      || fail "glibc 2.28 or newer is required (found $glibc_version)"
    target=${rust_arch}-unknown-linux-gnu
    ;;
  *) fail "unsupported OS '$os' (Linux and macOS only)" ;;
esac

asset=${BIN_NAME}-${target}

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/mach-install.XXXXXX")
manifest=${tmpdir}/${CHECKSUMS_ASSET}
release_json=${tmpdir}/release.json
dest_tmp=

cleanup() {
  if [ -n "$dest_tmp" ] && [ -e "$dest_tmp" ]; then
    rm -f "$dest_tmp"
  fi
  rm -f "$manifest" "$release_json"
  rmdir "$tmpdir" 2>/dev/null || :
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

tag=${MACH_VERSION:-}
if [ -z "$tag" ]; then
  download "$API_LATEST" "$release_json" "$MAX_TEXT_BYTES" \
    "GitHub's latest stable release metadata"
  tag=$(tr '\n' ' ' < "$release_json" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  [ -n "$tag" ] || fail "latest GitHub release did not contain a tag"
fi
case "$tag" in
  v*) : ;;
  *) tag=v${tag} ;;
esac
printf '%s\n' "$tag" \
  | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' \
  || fail "MACH_VERSION must be an exact stable semver tag (for example v0.2.0)"

expected_asset_url=${RELEASE_BASE}/${tag}/${asset}
expected_checksums_url=${RELEASE_BASE}/${tag}/${CHECKSUMS_ASSET}
asset_url=$expected_asset_url
checksums_url=$expected_checksums_url

mkdir -p "$INSTALL_DIR"
dest_tmp=$(mktemp "${INSTALL_DIR}/.mach.XXXXXX")

printf 'Downloading %s\n' "$asset_url"
download "$checksums_url" "$manifest" "$MAX_TEXT_BYTES" \
  "$CHECKSUMS_ASSET for $tag"
download "$asset_url" "$dest_tmp" "$MAX_BINARY_BYTES" "$asset for $tag"

if ! expected_sha=$(
  awk -v asset="$asset" '
    $2 == asset || $2 == "*" asset {
      count++
      if (NF != 2) invalid = 1
      digest = $1
    }
    END {
      if (count == 1 && !invalid) print digest
      else exit 1
    }
  ' "$manifest"
); then
  fail "$CHECKSUMS_ASSET must contain exactly one two-field entry for $asset"
fi
case "$expected_sha" in
  *[!0-9A-Fa-f]* | '') fail "$CHECKSUMS_ASSET has no valid entry for $asset" ;;
esac
[ "${#expected_sha}" -eq 64 ] || fail "$CHECKSUMS_ASSET has an invalid digest for $asset"

if command -v sha256sum >/dev/null 2>&1; then
  actual_sha=$(sha256sum "$dest_tmp" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  actual_sha=$(shasum -a 256 "$dest_tmp" | awk '{ print $1 }')
else
  fail "need 'sha256sum' or 'shasum' on PATH"
fi
expected_sha=$(printf '%s' "$expected_sha" | tr 'A-F' 'a-f')
actual_sha=$(printf '%s' "$actual_sha" | tr 'A-F' 'a-f')
[ "$actual_sha" = "$expected_sha" ] || fail "SHA-256 verification failed for $asset"

chmod 755 "$dest_tmp"
mv -f "$dest_tmp" "${INSTALL_DIR}/${BIN_NAME}"
dest_tmp=

printf 'Installed %s (%s)\n' "${INSTALL_DIR}/${BIN_NAME}" "$tag"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) : ;;
  *)
    printf 'Note: add %s to your PATH, for example:\n' "$INSTALL_DIR"
    printf "  export PATH=\"%s:\\$PATH\"\n" "$INSTALL_DIR"
    ;;
esac
printf 'Run: mach\n'
