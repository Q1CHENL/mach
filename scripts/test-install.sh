#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
installer=${repo_root}/install.sh
package_release=${repo_root}/scripts/package-release.sh
test_shell=${TEST_SHELL:-sh}
tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/mach-install-test.XXXXXX")

cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

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
version=${tag#v}
asset=mach-${target}.tar.gz
checksums=mach-${tag}-checksums.txt
release_dir=${tmpdir}/releases/${tag}
raw_dir=${tmpdir}/raw/${tag}
install_dir=${tmpdir}/bin
mkdir -p "$raw_dir" "$install_dir"

for build_target in \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-apple-darwin \
  aarch64-apple-darwin
do
  printf '#!/bin/sh\nprintf "fixture 9.8.7\\n"\n' > "${raw_dir}/mach-${build_target}"
  chmod 755 "${raw_dir}/mach-${build_target}"
done
"$package_release" "$version" "$raw_dir" "$release_dir"

MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
MACH_VERSION="$tag" \
MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null

cmp "${raw_dir}/mach-${target}" "${install_dir}/mach"
[ "$("${install_dir}/mach")" = 'fixture 9.8.7' ]
installed_sha=$(sha256_file "${install_dir}/mach")
receipt=${install_dir}/.mach-release-install/${installed_sha}
[ -f "$receipt" ] || {
  printf 'installer did not bind release ownership to the installed digest\n' >&2
  exit 1
}
if [ "$(wc -l < "$receipt" | tr -d '[:space:]')" != 1 ] \
  || [ "$(sed -n '1p' "$receipt")" != '9.8.7' ]; then
  printf 'installer receipt did not record the installed release version\n' >&2
  exit 1
fi

legacy_tag=v0.8.0
legacy_version=${legacy_tag#v}
legacy_release_dir=${tmpdir}/releases/${legacy_tag}
legacy_raw_dir=${tmpdir}/raw/${legacy_tag}
legacy_install_dir=${tmpdir}/legacy-bin
mkdir -p "$legacy_raw_dir" "$legacy_install_dir"
for build_target in \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-apple-darwin \
  aarch64-apple-darwin
do
  printf '#!/bin/sh\nprintf "fixture 0.8.0\\n"\n' > "${legacy_raw_dir}/mach-${build_target}"
  chmod 755 "${legacy_raw_dir}/mach-${build_target}"
done
"$package_release" "$legacy_version" "$legacy_raw_dir" "$legacy_release_dir"

MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
MACH_VERSION="$legacy_tag" \
MACH_INSTALL_DIR="$legacy_install_dir" \
  "$test_shell" "$installer" >/dev/null
cmp "${legacy_raw_dir}/mach-${target}" "${legacy_install_dir}/mach"
[ "$("${legacy_install_dir}/mach")" = 'fixture 0.8.0' ]

older_tag=v9.8.6
older_version=${older_tag#v}
older_release_dir=${tmpdir}/releases/${older_tag}
older_raw_dir=${tmpdir}/raw/${older_tag}
mkdir -p "$older_raw_dir"
for build_target in \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-apple-darwin \
  aarch64-apple-darwin
do
  printf '#!/bin/sh\nprintf "fixture 9.8.6\\n"\n' > "${older_raw_dir}/mach-${build_target}"
  chmod 755 "${older_raw_dir}/mach-${build_target}"
done
"$package_release" "$older_version" "$older_raw_dir" "$older_release_dir"

MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
MACH_VERSION="$older_tag" \
MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null
[ "$("${install_dir}/mach")" = 'fixture 9.8.7' ] || {
  printf 'installer downgraded a newer destination\n' >&2
  exit 1
}

concurrent_install_dir=${tmpdir}/concurrent-bin
mkdir -p "$concurrent_install_dir"
(
  MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$older_tag" \
  MACH_INSTALL_DIR="$concurrent_install_dir" \
    "$test_shell" "$installer" >/dev/null
) &
older_pid=$!
(
  MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$tag" \
  MACH_INSTALL_DIR="$concurrent_install_dir" \
    "$test_shell" "$installer" >/dev/null
) &
newer_pid=$!
wait "$older_pid"
wait "$newer_pid"
[ "$("${concurrent_install_dir}/mach")" = 'fixture 9.8.7' ] || {
  printf 'concurrent installers left the older release installed\n' >&2
  exit 1
}

directory_install_dir=${tmpdir}/directory-destination
mkdir -p "${directory_install_dir}/mach"
if MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$tag" \
  MACH_INSTALL_DIR="$directory_install_dir" \
  "$test_shell" "$installer" >/dev/null 2>&1; then
  printf 'installer accepted a directory as the executable destination\n' >&2
  exit 1
fi
[ ! -e "${directory_install_dir}/.mach-release-install/$(sha256_file "${release_dir}/${asset}")" ] \
  || {
    printf 'failed install left a release receipt for an uninstalled binary\n' >&2
    exit 1
  }

printf '{"node_id":"fixture","tag_name":"%s","name":"release"}' "$tag" \
  > "${tmpdir}/latest.json"
MACH_API_LATEST_URL="file://${tmpdir}/latest.json" \
MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null
cmp "${raw_dir}/mach-${target}" "${install_dir}/mach"

before=$(sha256_file "${install_dir}/mach")
cp "${release_dir}/${asset}" "${tmpdir}/${asset}.valid"
cp "${release_dir}/${checksums}" "${tmpdir}/${checksums}.valid"
printf 'corrupt\n' >> "${release_dir}/${asset}"
if MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$tag" \
  MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null 2>&1; then
  printf 'installer accepted a checksum mismatch\n' >&2
  exit 1
fi
after=$(sha256_file "${install_dir}/mach")
[ "$before" = "$after" ] || {
  printf 'failed verification replaced the installed binary\n' >&2
  exit 1
}

cp "${tmpdir}/${asset}.valid" "${release_dir}/${asset}"
cp "${tmpdir}/${checksums}.valid" "${release_dir}/${checksums}"
invalid_archive_dir=${tmpdir}/invalid-archive
mkdir -p "$invalid_archive_dir"
cp "${raw_dir}/mach-${target}" "${invalid_archive_dir}/mach"
printf 'unexpected\n' > "${invalid_archive_dir}/unexpected"
tar -czf "${release_dir}/${asset}" -C "$invalid_archive_dir" mach unexpected
invalid_archive_sha=$(sha256_file "${release_dir}/${asset}")
awk -v asset="$asset" -v digest="$invalid_archive_sha" '
  $2 == asset { $1 = digest }
  { print }
' "${release_dir}/${checksums}" > "${tmpdir}/${checksums}.invalid-archive"
mv "${tmpdir}/${checksums}.invalid-archive" "${release_dir}/${checksums}"
if MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$tag" \
  MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null 2>&1; then
  printf 'installer accepted an archive with extra entries\n' >&2
  exit 1
fi
after=$(sha256_file "${install_dir}/mach")
[ "$before" = "$after" ] || {
  printf 'invalid archive replaced the installed binary\n' >&2
  exit 1
}

cp "${tmpdir}/${asset}.valid" "${release_dir}/${asset}"
cp "${tmpdir}/${checksums}.valid" "${release_dir}/${checksums}"
malformed_sha=$(sha256_file "${release_dir}/${asset}")
printf '%s  %s  unexpected-field\n' "$malformed_sha" "$asset" \
  > "${release_dir}/${checksums}"
if MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$tag" \
  MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null 2>&1; then
  printf 'installer accepted a checksum entry with extra fields\n' >&2
  exit 1
fi
after=$(sha256_file "${install_dir}/mach")
[ "$before" = "$after" ] || {
  printf 'malformed checksum manifest replaced the installed binary\n' >&2
  exit 1
}

oversized_sha=$(sha256_file "${release_dir}/${asset}")
printf '%s  %s\n' "$oversized_sha" "$asset" > "${release_dir}/${checksums}"
awk 'BEGIN { for (i = 0; i < 1048577; i++) printf "#" }' \
  >> "${release_dir}/${checksums}"
if MACH_RELEASE_BASE_URL="file://${tmpdir}/releases" \
  MACH_VERSION="$tag" \
  MACH_INSTALL_DIR="$install_dir" \
  "$test_shell" "$installer" >/dev/null 2>&1; then
  printf 'installer accepted an oversized checksum manifest\n' >&2
  exit 1
fi
after=$(sha256_file "${install_dir}/mach")
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
