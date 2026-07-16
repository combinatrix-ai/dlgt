#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
installer="$repo_root/install.sh"
workflow="$repo_root/.github/workflows/release.yml"

sh -n "$installer"

DLGT_INSTALLER_NO_MAIN=1 . "$installer"

assert_target() {
  expected="$1"
  actual="$(detect_target "$2" "$3" "${4:-auto}")"
  [ "$actual" = "$expected" ] || {
    printf 'expected target %s, got %s\n' "$expected" "$actual" >&2
    exit 1
  }
}

assert_asset() {
  expected="$1"
  actual="$(release_asset_name "$2" "$3")"
  [ "$actual" = "$expected" ] || {
    printf 'expected asset %s, got %s\n' "$expected" "$actual" >&2
    exit 1
  }
}

assert_target aarch64-apple-darwin Darwin arm64
assert_target x86_64-apple-darwin Darwin amd64
assert_target aarch64-unknown-linux-gnu Linux aarch64 gnu
assert_target x86_64-unknown-linux-gnu Linux x86_64 gnu
assert_target aarch64-unknown-linux-musl Linux arm64 musl
assert_target x86_64-unknown-linux-musl Linux amd64 musl
assert_asset dlgt-v0.1.0-aarch64-apple-darwin.tar.gz v0.1.0 aarch64-apple-darwin

for version in v1.2.3 1.2.3 v1.2.3-rc.1; do
  (validate_version "$version") || {
    printf 'valid version was rejected: %s\n' "$version" >&2
    exit 1
  }
done

for version in v1.2 v1.2.3oops v1.foo.3 v1.2.3/evil; do
  if (validate_version "$version") >/dev/null 2>&1; then
    printf 'invalid version was accepted: %s\n' "$version" >&2
    exit 1
  fi
done

checksum_test_directory="$(mktemp -d "${TMPDIR:-/tmp}/dlgt-installer-test.XXXXXX")"
trap 'rm -rf "$checksum_test_directory"' 0 1 2 15
archive="$checksum_test_directory/dlgt.tar.gz"
checksum="$archive.sha256"
printf 'published dlgt archive\n' > "$archive"
printf '%s  %s\n' "$(sha256 "$archive")" "$(basename "$archive")" > "$checksum"
verify_checksum "$archive" "$checksum"
printf 'modified archive\n' > "$archive"
if (verify_checksum "$archive" "$checksum") >/dev/null 2>&1; then
  printf 'modified archive passed checksum verification\n' >&2
  exit 1
fi

if detect_target FreeBSD x86_64 auto >/dev/null 2>&1; then
  printf 'unsupported OS was accepted\n' >&2
  exit 1
fi

for target in \
  aarch64-apple-darwin \
  x86_64-apple-darwin \
  aarch64-unknown-linux-gnu \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-musl \
  x86_64-unknown-linux-musl; do
  grep -F "target: $target" "$workflow" >/dev/null || {
    printf 'release workflow is missing target %s\n' "$target" >&2
    exit 1
  }
done

grep -F 'dlgt-${GITHUB_REF_NAME}-${{ matrix.target }}.tar.gz' "$workflow" >/dev/null
grep -F 'dlgt-${tag}-checksums.txt' "$workflow" >/dev/null
grep -F 'cargo pkgid --package dlgt' "$workflow" >/dev/null
grep -F 'cargo build --release --locked --target "${{ matrix.target }}" --bin dlgt' "$workflow" >/dev/null

printf 'dlgt installer and release naming tests passed\n'
