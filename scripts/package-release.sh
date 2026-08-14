#!/bin/sh
set -eu

fail() {
  printf 'mach package: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "need '$1' on PATH"
}

need awk
need chmod
need cp
need dirname
need gzip
need grep
need mkdir
need mktemp
need mv
need rm
need tar
need touch

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

usage="usage: $0 VERSION RAW_DIST RELEASE_DIST"
[ "$#" -eq 3 ] || fail "$usage"

version=$1
raw_dist=$2
release_dist=$3

printf '%s\n' "$version" \
  | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' \
  || fail "invalid stable version: $version"
[ -d "$raw_dist" ] || fail "raw artifact directory does not exist: $raw_dist"
[ ! -e "$release_dist" ] \
  || fail "release artifact path already exists: $release_dist"
release_parent=$(dirname -- "$release_dist")
mkdir -p "$release_parent"
tmpdir=
staged_dist=
cleanup() {
  if [ -n "$tmpdir" ] && [ -e "$tmpdir" ]; then
    rm -rf "$tmpdir"
  fi
  if [ -n "$staged_dist" ] && [ -e "$staged_dist" ]; then
    rm -rf "$staged_dist"
  fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/mach-package.XXXXXX")
staged_dist=$(mktemp -d "${release_parent}/.mach-release.XXXXXX")

is_raw_asset() {
  case "$1" in
    mach-x86_64-unknown-linux-gnu \
      | mach-aarch64-unknown-linux-gnu \
      | mach-x86_64-apple-darwin \
      | mach-aarch64-apple-darwin) return 0 ;;
    *) return 1 ;;
  esac
}

raw_count=0
for raw_path in "$raw_dist"/*; do
  [ -e "$raw_path" ] || continue
  [ -f "$raw_path" ] && [ ! -L "$raw_path" ] \
    || fail "unexpected non-file raw artifact: $raw_path"
  is_raw_asset "${raw_path##*/}" \
    || fail "unexpected raw artifact: ${raw_path##*/}"
  raw_count=$((raw_count + 1))
done
[ "$raw_count" -eq 4 ] \
  || fail "expected four raw platform binaries, found $raw_count"

stage=${tmpdir}/stage
mkdir -p "$stage"
gnu_tar=false
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
  gnu_tar=true
fi

for target in \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-apple-darwin \
  aarch64-apple-darwin
do
  raw_asset=mach-${target}
  archive_asset=${raw_asset}.tar.gz
  raw_path=${raw_dist}/${raw_asset}
  [ -f "$raw_path" ] && [ ! -L "$raw_path" ] \
    || fail "missing raw platform binary: $raw_asset"

  cp "$raw_path" "${stage}/mach"
  chmod 755 "${stage}/mach"
  touch -t 198001010000.00 "${stage}/mach"

  uncompressed=${tmpdir}/${raw_asset}.tar
  if [ "$gnu_tar" = true ]; then
    tar \
      --sort=name \
      --format=ustar \
      --mtime='1980-01-01 00:00:00Z' \
      --owner=0 \
      --group=0 \
      --numeric-owner \
      -cf "$uncompressed" \
      -C "$stage" \
      mach
  else
    COPYFILE_DISABLE=1 tar --format ustar -cf "$uncompressed" -C "$stage" mach
  fi
  gzip -n -c "$uncompressed" > "${staged_dist}/${archive_asset}"
  rm -f "$uncompressed" "${stage}/mach"

  # Existing Mach versions require these exact assets to reach the archive-aware bridge release.
  cp "$raw_path" "${staged_dist}/${raw_asset}"
  chmod 755 "${staged_dist}/${raw_asset}"
done

checksums_asset=mach-v${version}-checksums.txt
: > "${staged_dist}/${checksums_asset}"
: > "${staged_dist}/SHA256SUMS"
for target in \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-apple-darwin \
  aarch64-apple-darwin
do
  raw_asset=mach-${target}
  archive_asset=${raw_asset}.tar.gz
  printf '%s  %s\n' \
    "$(sha256_file "${staged_dist}/${archive_asset}")" \
    "$archive_asset" \
    >> "${staged_dist}/${checksums_asset}"
  printf '%s  %s\n' \
    "$(sha256_file "${staged_dist}/${raw_asset}")" \
    "$raw_asset" \
    >> "${staged_dist}/SHA256SUMS"
done

mv "$staged_dist" "$release_dist"
staged_dist=
