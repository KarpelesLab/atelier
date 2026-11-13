#!/usr/bin/env bash
# Install the atelier release binary for this runner.
#
# Downloads `atelier-<target>.gz` and the release's `SHA256SUMS`, checks the
# one against the other, and only then decompresses. Anything that does not
# verify — a mismatch, an asset the list does not name, a list that could not
# be fetched — stops the step: nothing unverified is ever made executable.
#
# Environment:
#   INPUT_VERSION                a release tag (`v0.9.0`), or `latest` (default)
#   ATELIER_ACTION_RELEASES_URL  where releases live (a mirror; the tests)
#   RUNNER_TOOL_CACHE            the binary is kept there, by version and target
#   GITHUB_OUTPUT, GITHUB_PATH   receive `bin`/`version` and the directory
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=.github/action/lib.sh
. "$here/lib.sh"

releases=${ATELIER_ACTION_RELEASES_URL:-https://github.com/KarpelesLab/atelier/releases}
version=${INPUT_VERSION:-latest}

os=$(uname -s)
arch=$(uname -m)
[ "$os" = Linux ] ||
  die "atelier publishes Linux binaries only (x86_64, arm64); this runner is $os/$arch. Build from source instead: cargo install --git https://github.com/KarpelesLab/atelier"
case $arch in
  x86_64 | amd64) target=x86_64-unknown-linux-fullrust ;;
  aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
  *) die "atelier publishes binaries for Linux x86_64 and arm64 only; this runner is $os/$arch" ;;
esac

fetch() { # fetch URL FILE
  curl --fail --silent --show-error --location --retry 3 --output "$2" -- "$1"
}

if [ "$version" = latest ]; then
  # `releases/latest` redirects to `releases/tag/<tag>`: the tag is the last
  # path component of where the redirect ends. No API call, so no rate limit.
  final=$(curl --fail --silent --show-error --location --retry 3 \
    --output /dev/null --write-out '%{url_effective}' -- "$releases/latest") ||
    die "could not find the latest atelier release at $releases/latest"
  version=${final##*/}
fi
# The tag becomes part of a URL and of a path: it must look like one.
[[ $version =~ ^[0-9A-Za-z][0-9A-Za-z._+-]*$ ]] ||
  die "version: not a release tag (expected something like v0.9.0, or latest)"

asset="atelier-$target.gz"
cache="${RUNNER_TOOL_CACHE:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}/atelier/$version/$target"
bin="$cache/atelier"

if [ -x "$bin" ]; then
  log "atelier $version ($target): using the cached binary"
else
  tmp=$(mktemp -d)
  trap 'rm -rf -- "$tmp"' EXIT
  fetch "$releases/download/$version/$asset" "$tmp/$asset" ||
    die "could not download $asset of release $version"
  fetch "$releases/download/$version/SHA256SUMS" "$tmp/SHA256SUMS" ||
    die "could not download SHA256SUMS of release $version: refusing to install an unverified binary"

  # `sha256sum` lines are `<hash>  <name>` (or `<hash> *<name>`).
  expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1; exit }' "$tmp/SHA256SUMS")
  [[ $expected =~ ^[0-9a-f]{64}$ ]] ||
    die "SHA256SUMS of release $version has no checksum for $asset: refusing to install an unverified binary"
  actual=$(sha256sum <"$tmp/$asset" | cut -d' ' -f1)
  [ "$actual" = "$expected" ] ||
    die "checksum mismatch for $asset of release $version: expected $expected, got $actual"

  gunzip -c <"$tmp/$asset" >"$tmp/atelier" || die "$asset is not a gzip file"
  chmod +x "$tmp/atelier"
  mkdir -p "$cache"
  # Into place in one step: a concurrent job sees the whole binary or none.
  mv -f -- "$tmp/atelier" "$bin"
  log "atelier $version ($target): installed, checksum verified"
fi

set_output_value bin "$bin"
set_output_value version "$version"
if [ -n "${GITHUB_PATH:-}" ]; then
  printf '%s\n' "$cache" >>"$GITHUB_PATH"
fi
