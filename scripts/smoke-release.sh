#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
  printf 'usage: %s BINARY TARGET VERSION MAX_ABI\n' "$0" >&2
  exit 2
fi

binary=$1
target=$2
version=$3
max_abi=$4

[ -x "$binary" ] || {
  printf '%s is not executable\n' "$binary" >&2
  exit 1
}

version_line=$("$binary" --version | tail -n 1)
[ "$version_line" = "mach v${version}" ] || {
  printf 'version mismatch: expected mach v%s, got %s\n' "$version" "$version_line" >&2
  exit 1
}

version_at_most() {
  awk -v actual="$1" -v maximum="$2" 'BEGIN {
    split(actual, a, "."); split(maximum, m, ".");
    if ((a[1] + 0) < (m[1] + 0)) exit 0;
    if ((a[1] + 0) > (m[1] + 0)) exit 1;
    exit !((a[2] + 0) <= (m[2] + 0));
  }'
}

case "$target" in
  *-unknown-linux-gnu)
    case "$target" in
      x86_64-*) expected_machine='Advanced Micro Devices X86-64' ;;
      aarch64-*) expected_machine='AArch64' ;;
      *) printf 'unsupported Linux target %s\n' "$target" >&2; exit 1 ;;
    esac
    machine=$(readelf -h "$binary" | awk -F: '/Machine:/ { sub(/^[[:space:]]+/, "", $2); print $2 }')
    [ "$machine" = "$expected_machine" ] || {
      printf 'architecture mismatch: expected %s, got %s\n' "$expected_machine" "$machine" >&2
      exit 1
    }
    highest_glibc=$(
      readelf --version-info "$binary" \
        | grep -o 'GLIBC_[0-9][0-9.]*' \
        | sed 's/^GLIBC_//' \
        | sort -Vu \
        | tail -n 1
    )
    [ -n "$highest_glibc" ] || {
      printf 'could not determine GLIBC floor\n' >&2
      exit 1
    }
    version_at_most "$highest_glibc" "$max_abi" || {
      printf 'GLIBC floor %s exceeds supported maximum %s\n' "$highest_glibc" "$max_abi" >&2
      exit 1
    }
    ;;
  *-apple-darwin)
    case "$target" in
      x86_64-*) expected_arch=x86_64 ;;
      aarch64-*) expected_arch=arm64 ;;
      *) printf 'unsupported macOS target %s\n' "$target" >&2; exit 1 ;;
    esac
    lipo -archs "$binary" | tr ' ' '\n' | grep -Fx "$expected_arch" >/dev/null || {
      printf 'binary does not contain expected architecture %s\n' "$expected_arch" >&2
      exit 1
    }
    minimum_macos=$(
      otool -l "$binary" | awk '
        /cmd LC_BUILD_VERSION/ { build = 1; legacy = 0; next }
        /cmd LC_VERSION_MIN_MACOSX/ { legacy = 1; build = 0; next }
        build && /minos/ { print $2; exit }
        legacy && /version/ { print $2; exit }
      '
    )
    [ -n "$minimum_macos" ] || {
      printf 'could not determine minimum macOS version\n' >&2
      exit 1
    }
    version_at_most "$minimum_macos" "$max_abi" || {
      printf 'minimum macOS %s exceeds supported maximum %s\n' "$minimum_macos" "$max_abi" >&2
      exit 1
    }
    ;;
  *)
    printf 'unsupported target %s\n' "$target" >&2
    exit 1
    ;;
esac

printf 'release smoke passed: %s %s (ABI <= %s)\n' "$target" "$version" "$max_abi"
