#!/bin/sh
# Install one checksum-verified mach GitHub release into ~/.local/bin.
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
RECEIPT_NAME=.mach-release-install
LOCK_NAME=.mach-install.lock
LOCK_OWNER=owner
LOCK_WAIT_SECONDS=30

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
need date
need grep
need mkdir
need mktemp
need mv
need sed
need sleep
need tr
need uname
need wc

if command -v sha256sum >/dev/null 2>&1; then
  sha256_file() {
    sha256sum "$1" | awk '{ print $1 }'
  }
elif command -v shasum >/dev/null 2>&1; then
  sha256_file() {
    shasum -a 256 "$1" | awk '{ print $1 }'
  }
else
  fail "need 'sha256sum' or 'shasum' on PATH"
fi

is_stable_version() {
  printf '%s\n' "$1" \
    | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

version_is_at_least() {
  actual=$1
  required=$2
  is_stable_version "$actual" || return 1
  is_stable_version "$required" || return 1
  awk -v actual="$actual" -v required="$required" '
    function greater(left, right) {
      if (length(left) != length(right)) return length(left) > length(right)
      return ("x" left) > ("x" right)
    }
    BEGIN {
      split(actual, actual_parts, ".")
      split(required, required_parts, ".")
      for (part = 1; part <= 3; part++) {
        if (("x" actual_parts[part]) == ("x" required_parts[part])) continue
        exit !greater(actual_parts[part], required_parts[part])
      }
      exit 0
    }
  '
}

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

receipt_dir=${INSTALL_DIR}/${RECEIPT_NAME}
lock_dir=${INSTALL_DIR}/${LOCK_NAME}
lock_held=false
lock_token=
lock_record=
receipt_tmp=
receipt_changed=false
receipt_previous=

read_receipt_version() {
  receipt_path=$1
  receipt_value=$(awk '
    NR == 1 { value = $0; next }
    { invalid = 1 }
    END {
      if (NR == 1 && !invalid && value != "") print value
      else exit 1
    }
  ' "$receipt_path") || return 2
  is_stable_version "$receipt_value" \
    || return 2
  printf '%s\n' "$receipt_value"
}

receipted_destination_version() {
  receipt_destination=$1
  [ -f "$receipt_destination" ] || return 1
  [ -d "$receipt_dir" ] || return 1
  receipt_digest=$(sha256_file "$receipt_destination") || return 2
  receipt_path=${receipt_dir}/${receipt_digest}
  [ -f "$receipt_path" ] || return 1
  read_receipt_version "$receipt_path"
}

record_release_version() {
  receipt_digest=$1
  receipt_candidate=$2
  mkdir -p "$receipt_dir"
  receipt_path=${receipt_dir}/${receipt_digest}
  recorded_version=$receipt_candidate
  receipt_changed=false
  receipt_previous=
  if [ -f "$receipt_path" ]; then
    receipt_existing=$(read_receipt_version "$receipt_path") \
      || fail "invalid release receipt $receipt_path"
    if version_is_at_least "$receipt_existing" "$receipt_candidate"; then
      recorded_version=$receipt_existing
      return 0
    fi
    receipt_previous=$receipt_existing
  fi
  receipt_tmp=$(mktemp "${receipt_dir}/.receipt.XXXXXX")
  printf '%s\n' "$recorded_version" > "$receipt_tmp"
  chmod 644 "$receipt_tmp"
  mv -f "$receipt_tmp" "$receipt_path"
  receipt_tmp=
  receipt_changed=true
}

rollback_release_version() {
  [ "$receipt_changed" = true ] || return 0
  if [ -n "$receipt_previous" ]; then
    receipt_tmp=$(mktemp "${receipt_dir}/.receipt.XXXXXX") || return 1
    printf '%s\n' "$receipt_previous" > "$receipt_tmp" || return 1
    chmod 644 "$receipt_tmp" || return 1
    mv -f "$receipt_tmp" "$receipt_path" || return 1
    receipt_tmp=
  else
    rm -f "$receipt_path" || return 1
  fi
  receipt_changed=false
}

replace_installed_binary() {
  if mv -f "$dest_tmp" "$destination"; then
    dest_tmp=
    receipt_changed=false
    return 0
  fi
  rollback_release_version \
    || fail "could not replace $destination or roll back its release receipt"
  fail "could not replace $destination atomically"
}

read_install_lock_record() {
  [ -f "${lock_dir}/${LOCK_OWNER}" ] || return 1
  sed -n '1p' "${lock_dir}/${LOCK_OWNER}"
}

acquire_install_lock() {
  lock_started=$(date +%s)
  lock_token=$$.${lock_started}
  while ! mkdir "$lock_dir" 2>/dev/null; do
    lock_now=$(date +%s)
    [ $((lock_now - lock_started)) -lt "$LOCK_WAIT_SECONDS" ] \
      || fail "timed out waiting for another installer holding $lock_dir; if no installer is running, remove this stale lock directory"
    sleep 1
  done

  lock_record="$(date +%s) $lock_token"
  if ! printf '%s\n' "$lock_record" > "${lock_dir}/${LOCK_OWNER}"; then
    rm -f "${lock_dir}/${LOCK_OWNER}" 2>/dev/null || :
    rmdir "$lock_dir" 2>/dev/null || :
    fail "could not record install lock ownership"
  fi
  lock_held=true
}

release_install_lock() {
  [ "$lock_held" = true ] || return 0
  lock_current=$(read_install_lock_record 2>/dev/null || :)
  if [ "$lock_current" = "$lock_record" ]; then
    rm -f "${lock_dir}/${LOCK_OWNER}"
    rmdir "$lock_dir" 2>/dev/null || :
  fi
  lock_held=false
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
  rollback_release_version 2>/dev/null || :
  release_install_lock
  if [ -n "$dest_tmp" ] && [ -e "$dest_tmp" ]; then
    rm -f "$dest_tmp"
  fi
  if [ -n "$receipt_tmp" ] && [ -e "$receipt_tmp" ]; then
    rm -f "$receipt_tmp"
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
target_version=${tag#v}

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

actual_sha=$(sha256_file "$dest_tmp")
expected_sha=$(printf '%s' "$expected_sha" | tr 'A-F' 'a-f')
actual_sha=$(printf '%s' "$actual_sha" | tr 'A-F' 'a-f')
[ "$actual_sha" = "$expected_sha" ] || fail "SHA-256 verification failed for $asset"

chmod 755 "$dest_tmp"
destination=${INSTALL_DIR}/${BIN_NAME}
acquire_install_lock
[ ! -d "$destination" ] \
  || fail "install destination is a directory: $destination"
install_action=Installed
installed_version=$target_version
if installed_version=$(receipted_destination_version "$destination"); then
  if version_is_at_least "$installed_version" "$target_version"; then
    install_action=Kept
    rm -f "$dest_tmp"
    dest_tmp=
  else
    record_release_version "$actual_sha" "$target_version"
    replace_installed_binary
    installed_version=$recorded_version
  fi
else
  receipt_status=$?
  [ "$receipt_status" -eq 1 ] \
    || fail "installed release receipt is invalid"
  record_release_version "$actual_sha" "$target_version"
  replace_installed_binary
  installed_version=$recorded_version
fi
release_install_lock

printf '%s %s (v%s)\n' "$install_action" "$destination" "$installed_version"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) : ;;
  *)
    printf 'Note: add %s to your PATH, for example:\n' "$INSTALL_DIR"
    printf "  export PATH=\"%s:\\$PATH\"\n" "$INSTALL_DIR"
    ;;
esac
printf 'Run: mach\n'
