#!/bin/sh
set -eu

fail() {
  printf 'mach release: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "need '$1' on PATH"
}

need awk
need grep
need jq
need tr
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

file_size() {
  size=$(wc -c < "$1" | tr -d '[:space:]')
  case "$size" in
    '' | *[!0-9]*) fail "could not measure $1" ;;
  esac
  printf '%s\n' "$size"
}

is_binary_asset() {
  case "$1" in
    mach-x86_64-unknown-linux-gnu \
      | mach-aarch64-unknown-linux-gnu \
      | mach-x86_64-apple-darwin \
      | mach-aarch64-apple-darwin) return 0 ;;
    *) return 1 ;;
  esac
}

is_release_asset() {
  is_binary_asset "$1" || [ "$1" = SHA256SUMS ]
}

verify_local() {
  dist=$1
  [ -d "$dist" ] || fail "artifact directory does not exist: $dist"

  asset_count=0
  for path in "$dist"/*; do
    [ -e "$path" ] || continue
    [ -f "$path" ] || fail "unexpected non-file artifact: $path"
    asset=${path##*/}
    is_release_asset "$asset" || fail "unexpected local artifact: $asset"
    asset_count=$((asset_count + 1))
  done
  [ "$asset_count" -eq 5 ] \
    || fail "expected five local release assets, found $asset_count"

  for asset in \
    mach-x86_64-unknown-linux-gnu \
    mach-aarch64-unknown-linux-gnu \
    mach-x86_64-apple-darwin \
    mach-aarch64-apple-darwin \
    SHA256SUMS
  do
    [ -f "${dist}/${asset}" ] || fail "missing local release asset: $asset"
  done

  manifest_count=0
  seen_assets=' '
  while IFS=' ' read -r expected_sha asset extra; do
    [ -z "${extra:-}" ] \
      || fail 'SHA256SUMS entries must contain exactly two fields'
    asset=${asset#\*}
    is_binary_asset "$asset" \
      || fail "SHA256SUMS contains an unexpected asset: $asset"
    case "$expected_sha" in
      '' | *[!0-9A-Fa-f]*) fail "SHA256SUMS has an invalid digest for $asset" ;;
    esac
    [ "${#expected_sha}" -eq 64 ] \
      || fail "SHA256SUMS has an invalid digest for $asset"
    case "$seen_assets" in
      *" $asset "*) fail "SHA256SUMS contains duplicate entries for $asset" ;;
    esac
    seen_assets="${seen_assets}${asset} "
    actual_sha=$(sha256_file "${dist}/${asset}")
    expected_sha=$(printf '%s' "$expected_sha" | tr 'A-F' 'a-f')
    [ "$actual_sha" = "$expected_sha" ] \
      || fail "SHA-256 verification failed for $asset"
    manifest_count=$((manifest_count + 1))
  done < "${dist}/SHA256SUMS"
  [ "$manifest_count" -eq 4 ] \
    || fail "SHA256SUMS must contain exactly four binary entries"
}

verify_github() {
  dist=$1
  release_json=$2
  notes_path=$3
  expected_draft=$4
  release_label=$5
  verify_local "$dist"
  [ -f "$release_json" ] || fail "release JSON does not exist: $release_json"
  [ -s "$notes_path" ] || fail "release notes do not exist or are empty: $notes_path"

  jq -e --argjson expected_draft "$expected_draft" '
    .draft == $expected_draft
    and .prerelease == false
    and ([.assets[].name] | sort == [
      "SHA256SUMS",
      "mach-aarch64-apple-darwin",
      "mach-aarch64-unknown-linux-gnu",
      "mach-x86_64-apple-darwin",
      "mach-x86_64-unknown-linux-gnu"
    ])
  ' "$release_json" >/dev/null \
    || fail "GitHub $release_label does not contain exactly the expected release assets"

  expected_body=$(cat "$notes_path")
  remote_body=$(jq -er '.body // ""' "$release_json")
  [ "$remote_body" = "$expected_body" ] \
    || fail "GitHub $release_label notes do not match $notes_path"

  for asset in \
    mach-x86_64-unknown-linux-gnu \
    mach-aarch64-unknown-linux-gnu \
    mach-x86_64-apple-darwin \
    mach-aarch64-apple-darwin \
    SHA256SUMS
  do
    remote_size=$(jq -er --arg name "$asset" \
      '.assets[] | select(.name == $name) | .size' "$release_json")
    remote_digest=$(jq -er --arg name "$asset" \
      '.assets[] | select(.name == $name) | .digest' "$release_json")
    remote_state=$(jq -er --arg name "$asset" \
      '.assets[] | select(.name == $name) | .state' "$release_json")
    local_size=$(file_size "${dist}/${asset}")
    local_digest=sha256:$(sha256_file "${dist}/${asset}")
    [ "$remote_size" = "$local_size" ] \
      || fail "uploaded size does not match $asset"
    [ "$remote_digest" = "$local_digest" ] \
      || fail "uploaded digest does not match $asset"
    [ "$remote_state" = uploaded ] \
      || fail "GitHub asset is not uploaded: $asset"
  done
}

verify_crate() {
  crate_path=$1
  expected_version=$2
  version_json=$3
  [ -f "$crate_path" ] || fail "crate archive does not exist: $crate_path"
  [ -f "$version_json" ] || fail "crate version JSON does not exist: $version_json"
  validate_stable_version "$expected_version"

  jq -e --arg version "$expected_version" '
    .version.num == $version
    and .version.yanked == false
    and (.version.checksum | type == "string")
  ' "$version_json" >/dev/null \
    || fail "crates.io did not return active version $expected_version"
  remote_checksum=$(jq -er '.version.checksum' "$version_json")
  local_checksum=$(sha256_file "$crate_path")
  [ "$remote_checksum" = "$local_checksum" ] \
    || fail "crates.io checksum does not match $crate_path"
}

validate_stable_version() {
  printf '%s\n' "$1" \
    | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' \
    || fail "invalid stable version: $1"
}

verify_newer() {
  candidate=$1
  published=$2
  validate_stable_version "$candidate"
  validate_stable_version "$published"
  awk -v candidate="$candidate" -v published="$published" '
    function greater(left, right) {
      if (length(left) != length(right)) return length(left) > length(right)
      return ("x" left) > ("x" right)
    }
    BEGIN {
      split(candidate, candidate_parts, ".")
      split(published, published_parts, ".")
      for (part = 1; part <= 3; part++) {
        if (("x" candidate_parts[part]) == ("x" published_parts[part])) continue
        exit !greater(candidate_parts[part], published_parts[part])
      }
      exit 1
    }
  ' || fail "version $candidate must be newer than $published"
}

verify_notes() {
  notes_path=$1
  version=$2
  previous_version=$3
  # Match the typographic heading used in the published GitHub notes.
  # shellcheck disable=SC1112
  notes_heading='## What’s new'
  validate_stable_version "$version"
  verify_newer "$version" "$previous_version"
  [ -f "$notes_path" ] || fail "release notes do not exist: $notes_path"
  [ -s "$notes_path" ] || fail "release notes are empty: $notes_path"
  [ "${notes_path##*/}" = "v${version}.md" ] \
    || fail "release-notes filename must be v${version}.md"

  first_line=$(awk 'NR == 1 { print; exit }' "$notes_path")
  [ "$first_line" = "$notes_heading" ] \
    || fail "release notes must start with: $notes_heading"
  grep -Eq '^- \*\*[^*]+:\*\* .+' "$notes_path" \
    || fail 'release notes must contain at least one user-visible highlight'

  changelog="**Full Changelog:** https://github.com/Q1CHENL/mach/compare/v${previous_version}...v${version}"
  grep -Fqx "$changelog" "$notes_path" \
    || fail "release notes must contain the exact changelog range from v${previous_version} to v${version}"
  changelog_count=$(awk '/^\*\*Full Changelog:\*\*/ { count++ } END { print count + 0 }' "$notes_path")
  [ "$changelog_count" -eq 1 ] \
    || fail 'release notes must contain exactly one Full Changelog line'
}

usage="usage: $0 local DIST | notes FILE VERSION PREVIOUS | github DIST RELEASE_JSON NOTES_FILE | github-published DIST RELEASE_JSON NOTES_FILE | crate ARCHIVE VERSION VERSION_JSON | newer VERSION PREVIOUS"

if [ "$#" -lt 2 ]; then
  fail "$usage"
fi

mode=$1
shift
case "$mode:$#" in
  local:1) verify_local "$1" ;;
  notes:3) verify_notes "$1" "$2" "$3" ;;
  github:3) verify_github "$1" "$2" "$3" true draft ;;
  github-published:3) verify_github "$1" "$2" "$3" false release ;;
  crate:3) verify_crate "$1" "$2" "$3" ;;
  newer:2) verify_newer "$1" "$2" ;;
  *) fail "$usage" ;;
esac
