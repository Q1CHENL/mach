#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
installer=${repo_root}/install.sh
test_shell=${TEST_SHELL:-sh}
tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/mach-install-test.XXXXXX")

cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

case "$(uname -m)" in
  x86_64 | amd64) rust_arch=x86_64 ;;
  arm64 | aarch64) rust_arch=aarch64 ;;
  *) printf 'unsupported test architecture\n' >&2; exit 1 ;;
esac
case "$(uname -s)" in
  Darwin) target=${rust_arch}-apple-darwin ;;
  Linux) target=${rust_arch}-unknown-linux-gnu ;;
  *) printf 'unsupported test OS\n' >&2; exit 1 ;;
esac

tag=v9.8.7
asset=mach-${target}
release_dir=${tmpdir}/releases/${tag}
install_dir=${tmpdir}/bin
mkdir -p "$release_dir" "$install_dir"

printf '#!/bin/sh\nprintf "fixture 9.8.7\\n"\n' > "${release_dir}/${asset}"
chmod 755 "${release_dir}/${asset}"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$release_dir" && sha256sum "$asset" > SHA256SUMS)
else
  (cd "$release_dir" && shasum -a 256 "$asset" > SHA256SUMS)
fi

MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
MACH_VERSION="$tag" \
MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null

cmp "${release_dir}/${asset}" "${install_dir}/mach"
[ "$("${install_dir}/mach")" = 'fixture 9.8.7' ]

printf '{"node_id":"fixture","tag_name":"%s","name":"release"}' "$tag" \
  > "${tmpdir}/latest.json"
MACH_API_LATEST_URL="file://${tmpdir}/latest.json" \
MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null
cmp "${release_dir}/${asset}" "${install_dir}/mach"

before=$(
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${install_dir}/mach" | awk '{ print $1 }'
  else
    shasum -a 256 "${install_dir}/mach" | awk '{ print $1 }'
  fi
)
printf 'corrupt\n' >> "${release_dir}/${asset}"
if MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$tag" \
  MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null 2>&1; then
  printf 'installer accepted a checksum mismatch\n' >&2
  exit 1
fi
after=$(
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${install_dir}/mach" | awk '{ print $1 }'
  else
    shasum -a 256 "${install_dir}/mach" | awk '{ print $1 }'
  fi
)
[ "$before" = "$after" ] || {
  printf 'failed verification replaced the installed binary\n' >&2
  exit 1
}

if command -v sha256sum >/dev/null 2>&1; then
  malformed_sha=$(sha256sum "${release_dir}/${asset}" | awk '{ print $1 }')
else
  malformed_sha=$(shasum -a 256 "${release_dir}/${asset}" | awk '{ print $1 }')
fi
printf '%s  %s  unexpected-field\n' "$malformed_sha" "$asset" \
  > "${release_dir}/SHA256SUMS"
if MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$tag" \
  MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null 2>&1; then
  printf 'installer accepted a checksum entry with extra fields\n' >&2
  exit 1
fi
after=$(
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${install_dir}/mach" | awk '{ print $1 }'
  else
    shasum -a 256 "${install_dir}/mach" | awk '{ print $1 }'
  fi
)
[ "$before" = "$after" ] || {
  printf 'malformed checksum manifest replaced the installed binary\n' >&2
  exit 1
}

if command -v sha256sum >/dev/null 2>&1; then
  oversized_sha=$(sha256sum "${release_dir}/${asset}" | awk '{ print $1 }')
else
  oversized_sha=$(shasum -a 256 "${release_dir}/${asset}" | awk '{ print $1 }')
fi
printf '%s  %s\n' "$oversized_sha" "$asset" > "${release_dir}/SHA256SUMS"
awk 'BEGIN { for (i = 0; i < 1048577; i++) printf "#" }' \
  >> "${release_dir}/SHA256SUMS"
if MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$tag" \
  MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null 2>&1; then
  printf 'installer accepted an oversized checksum manifest\n' >&2
  exit 1
fi
after=$(
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${install_dir}/mach" | awk '{ print $1 }'
  else
    shasum -a 256 "${install_dir}/mach" | awk '{ print $1 }'
  fi
)
[ "$before" = "$after" ] || {
  printf 'oversized checksum manifest replaced the installed binary\n' >&2
  exit 1
}

for invalid_version in v9.8 v09.8.7 v9.8.7+build v9.8.7-rc.1; do
  if MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
    MACH_VERSION="$invalid_version" \
    MACH_INSTALL_DIR="$install_dir" \
    "$test_shell" "$installer" >/dev/null 2>&1; then
    printf 'installer accepted invalid stable tag %s\n' "$invalid_version" >&2
    exit 1
  fi
done

printf 'installer contract passed for %s under %s\n' "$target" "$test_shell"
