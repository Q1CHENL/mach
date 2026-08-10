#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
verify_release=${repo_root}/scripts/verify-release.sh
tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/mach-release-test.XXXXXX")

cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

command -v jq >/dev/null 2>&1 || {
  printf "need 'jq' on PATH\n" >&2
  exit 1
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

expect_failure() {
  label=$1
  shift
  if "$@" >/dev/null 2>&1; then
    printf 'release contract accepted %s\n' "$label" >&2
    exit 1
  fi
}

dist=${tmpdir}/dist
mkdir -p "$dist"
for target in \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-apple-darwin \
  aarch64-apple-darwin
do
  printf 'fixture for %s\n' "$target" > "${dist}/mach-${target}"
done

(
  cd "$dist"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum mach-* > SHA256SUMS
  else
    shasum -a 256 mach-* > SHA256SUMS
  fi
)

"$verify_release" local "$dist"

cp "${dist}/SHA256SUMS" "${tmpdir}/SHA256SUMS.valid"
awk 'BEGIN { for (i = 0; i < 64; i++) printf "0" }' > "${dist}/SHA256SUMS"
printf '  mach-x86_64-unknown-linux-gnu\n' >> "${dist}/SHA256SUMS"
expect_failure 'a mismatched local checksum manifest' \
  "$verify_release" local "$dist"
cp "${tmpdir}/SHA256SUMS.valid" "${dist}/SHA256SUMS"

assets=${tmpdir}/assets.jsonl
for asset in \
  mach-x86_64-unknown-linux-gnu \
  mach-aarch64-unknown-linux-gnu \
  mach-x86_64-apple-darwin \
  mach-aarch64-apple-darwin \
  SHA256SUMS
do
  path=${dist}/${asset}
  size=$(wc -c < "$path" | tr -d '[:space:]')
  digest=sha256:$(sha256_file "$path")
  jq -n \
    --arg name "$asset" \
    --arg digest "$digest" \
    --argjson size "$size" \
    '{name: $name, size: $size, digest: $digest, state: "uploaded"}' \
    >> "$assets"
done
jq -s '{draft: true, prerelease: false, assets: .}' "$assets" \
  > "${tmpdir}/release.json"

"$verify_release" github "$dist" "${tmpdir}/release.json"

jq '.draft = false' "${tmpdir}/release.json" > "${tmpdir}/published.json"
"$verify_release" github-published "$dist" "${tmpdir}/published.json"
expect_failure 'an already-published GitHub release' \
  "$verify_release" github "$dist" "${tmpdir}/published.json"

jq '.assets[0].digest = "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"' \
  "${tmpdir}/release.json" > "${tmpdir}/wrong-asset.json"
expect_failure 'a GitHub asset with the wrong digest' \
  "$verify_release" github "$dist" "${tmpdir}/wrong-asset.json"
jq '.draft = false' "${tmpdir}/wrong-asset.json" \
  > "${tmpdir}/wrong-published-asset.json"
expect_failure 'a published GitHub asset with the wrong digest' \
  "$verify_release" github-published "$dist" \
  "${tmpdir}/wrong-published-asset.json"

jq '.assets += [{name: "unexpected", size: 1, digest: "sha256:00", state: "uploaded"}]' \
  "${tmpdir}/release.json" > "${tmpdir}/unexpected-asset.json"
expect_failure 'an unexpected GitHub release asset' \
  "$verify_release" github "$dist" "${tmpdir}/unexpected-asset.json"

crate=${tmpdir}/mach-tui-0.2.0.crate
printf 'crate fixture\n' > "$crate"
crate_checksum=$(sha256_file "$crate")
jq -n \
  --arg checksum "$crate_checksum" \
  '{version: {num: "0.2.0", checksum: $checksum, yanked: false}}' \
  > "${tmpdir}/crate.json"

"$verify_release" crate "$crate" 0.2.0 "${tmpdir}/crate.json"

jq '.version.yanked = true' "${tmpdir}/crate.json" > "${tmpdir}/yanked.json"
expect_failure 'a yanked crates.io version' \
  "$verify_release" crate "$crate" 0.2.0 "${tmpdir}/yanked.json"

jq '.version.checksum = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"' \
  "${tmpdir}/crate.json" > "${tmpdir}/wrong-crate.json"
expect_failure 'a crates.io version with the wrong checksum' \
  "$verify_release" crate "$crate" 0.2.0 "${tmpdir}/wrong-crate.json"

"$verify_release" newer 0.2.0 0.1.1
"$verify_release" newer 1.0.0 0.99.99
"$verify_release" newer 0.10.0 0.9.99
"$verify_release" newer 10000000000000000000.0.0 9999999999999999999.999.999
expect_failure 'a repeated release version' \
  "$verify_release" newer 0.2.0 0.2.0
expect_failure 'a release version older than the published version' \
  "$verify_release" newer 0.1.9 0.2.0
expect_failure 'a noncanonical release version' \
  "$verify_release" newer 0.2.0-rc.1 0.1.1

printf 'release contract passed\n'
