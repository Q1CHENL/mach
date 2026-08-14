#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
verify_release=${repo_root}/scripts/verify-release.sh
package_release=${repo_root}/scripts/package-release.sh
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

version=0.2.0
raw_dist=${tmpdir}/raw-dist
dist=${tmpdir}/dist
repeat_dist=${tmpdir}/repeat-dist
mkdir -p "$raw_dist"
for target in \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-apple-darwin \
  aarch64-apple-darwin
do
  printf 'fixture for %s\n' "$target" > "${raw_dist}/mach-${target}"
  chmod 755 "${raw_dist}/mach-${target}"
done

"$package_release" "$version" "$raw_dist" "$dist"
"$package_release" "$version" "$raw_dist" "$repeat_dist"
for path in "$dist"/*; do
  cmp "$path" "${repeat_dist}/${path##*/}"
done

incomplete_raw=${tmpdir}/incomplete-raw
failed_dist=${tmpdir}/failed-dist
mkdir -p "$incomplete_raw"
cp "${raw_dist}/mach-x86_64-unknown-linux-gnu" "$incomplete_raw"
expect_failure 'an incomplete raw artifact set' \
  "$package_release" "$version" "$incomplete_raw" "$failed_dist"
[ ! -e "$failed_dist" ] || {
  printf 'failed packaging left a partial release directory\n' >&2
  exit 1
}

"$verify_release" local "$dist" "$version"

notes_dir=${tmpdir}/release-notes
mkdir -p "$notes_dir"
notes=${notes_dir}/v0.2.0.md
# Match the typographic heading used in the published GitHub notes.
# shellcheck disable=SC1112
notes_heading='## What’s new'
{
  printf '%s\n' "$notes_heading"
  printf '\n'
  printf '%s\n' '- **Release notes contract:** Ship user-visible highlights from source control.'
  printf '\n'
  printf '%s\n' '**Full Changelog:** https://github.com/Q1CHENL/mach/compare/v0.1.1...v0.2.0'
} > "$notes"

"$verify_release" notes "$notes" 0.2.0 0.1.1
expect_failure 'a missing release-notes file' \
  "$verify_release" notes "${notes_dir}/v0.2.1.md" 0.2.1 0.2.0
cp "$notes" "${notes_dir}/wrong-name.md"
expect_failure 'a release-notes filename that does not match the version' \
  "$verify_release" notes "${notes_dir}/wrong-name.md" 0.2.0 0.1.1
sed 's/v0\.1\.1\.\.\.v0\.2\.0/v0.1.0...v0.2.0/' "$notes" \
  > "${notes_dir}/wrong-changelog.md"
expect_failure 'a release-notes file with the wrong changelog range' \
  "$verify_release" notes "${notes_dir}/wrong-changelog.md" 0.2.0 0.1.1
{
  printf '%s\n' "$notes_heading"
  printf '\n'
  printf '%s\n' '**Full Changelog:** https://github.com/Q1CHENL/mach/compare/v0.1.1...v0.2.0'
} > "${notes_dir}/v0.2.1.md"
expect_failure 'release notes without a user-visible highlight' \
  "$verify_release" notes "${notes_dir}/v0.2.1.md" 0.2.1 0.2.0

checksums=mach-v${version}-checksums.txt
cp "${dist}/${checksums}" "${tmpdir}/${checksums}.valid"
awk 'BEGIN { for (i = 0; i < 64; i++) printf "0" }' > "${dist}/${checksums}"
printf '  mach-x86_64-unknown-linux-gnu.tar.gz\n' >> "${dist}/${checksums}"
expect_failure 'a mismatched local checksum manifest' \
  "$verify_release" local "$dist" "$version"
cp "${tmpdir}/${checksums}.valid" "${dist}/${checksums}"

assets=${tmpdir}/assets.jsonl
for asset in \
  mach-x86_64-unknown-linux-gnu.tar.gz \
  mach-aarch64-unknown-linux-gnu.tar.gz \
  mach-x86_64-apple-darwin.tar.gz \
  mach-aarch64-apple-darwin.tar.gz \
  mach-x86_64-unknown-linux-gnu \
  mach-aarch64-unknown-linux-gnu \
  mach-x86_64-apple-darwin \
  mach-aarch64-apple-darwin \
  "$checksums" \
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
jq -s --rawfile body "$notes" \
  '{draft: true, prerelease: false, body: ($body | rtrimstr("\n")), assets: .}' \
  "$assets" \
  > "${tmpdir}/release.json"

"$verify_release" github "$dist" "${tmpdir}/release.json" "$notes" "$version"

jq '.draft = false' "${tmpdir}/release.json" > "${tmpdir}/published.json"
"$verify_release" github-published "$dist" "${tmpdir}/published.json" "$notes" "$version"
expect_failure 'an already-published GitHub release' \
  "$verify_release" github "$dist" "${tmpdir}/published.json" "$notes" "$version"

jq '.body = "Generated notes replaced the curated release notes."' \
  "${tmpdir}/release.json" > "${tmpdir}/wrong-body.json"
expect_failure 'a GitHub release with the wrong notes' \
  "$verify_release" github "$dist" "${tmpdir}/wrong-body.json" "$notes" "$version"

jq '.assets[0].digest = "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"' \
  "${tmpdir}/release.json" > "${tmpdir}/wrong-asset.json"
expect_failure 'a GitHub asset with the wrong digest' \
  "$verify_release" github "$dist" "${tmpdir}/wrong-asset.json" "$notes" "$version"
jq '.draft = false' "${tmpdir}/wrong-asset.json" \
  > "${tmpdir}/wrong-published-asset.json"
expect_failure 'a published GitHub asset with the wrong digest' \
  "$verify_release" github-published "$dist" \
  "${tmpdir}/wrong-published-asset.json" "$notes" "$version"

jq '.assets += [{name: "unexpected", size: 1, digest: "sha256:00", state: "uploaded"}]' \
  "${tmpdir}/release.json" > "${tmpdir}/unexpected-asset.json"
expect_failure 'an unexpected GitHub release asset' \
  "$verify_release" github "$dist" "${tmpdir}/unexpected-asset.json" "$notes" "$version"

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
