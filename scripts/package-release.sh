#!/bin/sh
# Package a checked native build. This script never publishes a release.
set -eu
TZ=UTC
export TZ

mode=${1:-}
version=${2:-}
target=${3:-}
case "$mode" in
  check) test "$#" -eq 3 ;;
  package) test "$#" -eq 6 ;;
  *) echo 'Usage: package-release.sh check VERSION TARGET | package VERSION TARGET EPOCH BINARY OUTPUT_DIRECTORY' >&2; exit 2 ;;
esac
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'
manifest_version=$(awk '/^\[package\]/{package=1;next} /^\[/{package=0} package && /^version = /{gsub(/"/, "", $3);print $3;exit}' Cargo.toml)
test "$version" = "$manifest_version" || { echo 'Release version does not match Cargo.toml.' >&2; exit 1; }
case "$target" in
  aarch64-apple-darwin|x86_64-apple-darwin|aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu) ;;
  *) echo 'Unsupported release target.' >&2; exit 1 ;;
esac
host=$(rustc -vV | sed -n 's/^host: //p')
test "$target" = "$host" || { echo 'Release target must match the native Rust host.' >&2; exit 1; }
test "$mode" = package || exit 0

epoch=$4
binary=$5
output=$6
printf '%s\n' "$epoch" | grep -Eq '^[0-9]{1,12}$'
if command -v sha256sum >/dev/null 2>&1; then
  lock_hash=$(sha256sum Cargo.lock | cut -d ' ' -f 1)
else
  lock_hash=$(shasum -a 256 Cargo.lock | cut -d ' ' -f 1)
fi
license_lock_hash=$(sed -n 's/^Cargo.lock SHA-256: //p' THIRD_PARTY_LICENSES.txt)
test "$lock_hash" = "$license_lock_hash" || { echo 'Third-party license bundle does not match Cargo.lock.' >&2; exit 1; }
test -x "$binary"
test "$("$binary" --version)" = "latchrun $version" || { echo 'Binary version mismatch.' >&2; exit 1; }
"$binary" --help >/dev/null
case "$target" in
  *-apple-darwin)
    test "${MACOSX_DEPLOYMENT_TARGET:-}" = 15.0 || { echo 'macOS release builds require deployment target 15.0.' >&2; exit 1; }
    otool -l "$binary" | awk '/minos/{if ($2 == "15.0") found=1} END{exit !found}'
    case "$target" in
      aarch64-*) test "$(lipo -archs "$binary")" = arm64 ;;
      x86_64-*) test "$(lipo -archs "$binary")" = x86_64 ;;
    esac
    stamp=$(date -u -r "$epoch" '+%Y%m%d%H%M.%S')
    ;;
  *-unknown-linux-gnu)
    case "$target" in
      aarch64-*) readelf -h "$binary" | grep -Eq 'Machine:[[:space:]]+AArch64$' ;;
      x86_64-*) readelf -h "$binary" | grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64$' ;;
    esac
    printf 'Build libc: %s\n' "$(getconf GNU_LIBC_VERSION)"
    printf 'Required glibc symbols through: %s\n' "$(readelf --version-info "$binary" | grep -o 'GLIBC_[0-9.]*' | LC_ALL=C sort -Vu | tail -n 1)"
    stamp=$(date -u -d "@$epoch" '+%Y%m%d%H%M.%S')
    ;;
esac

archive="latchrun-v${version}-${target}.tar.gz"
mkdir -p "$output"
test ! -e "$output/$archive"
test ! -e "$output/$archive.sha256"
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
trap 'exit 1' HUP INT TERM
mkdir "$staging/content"
cp "$binary" "$staging/content/latchrun"
cp LICENSE README.md THIRD_PARTY_LICENSES.txt CHANGELOG.md BUILD_BRIEF.md AGENTS.md "$staging/content/"
cp -R docs "$staging/content/docs"
cp -R examples "$staging/content/examples"
find "$staging/content" -type d -exec chmod 755 {} +
find "$staging/content" -type f -exec chmod 644 {} +
chmod 755 "$staging/content/latchrun"
find "$staging/content" -exec touch -t "$stamp" {} +

# A fixed entry order, ownership, timestamps, and gzip header make repackaging
# the same binary/docs deterministic. This is not a bit-identical compiler claim.
(
  cd "$staging/content"
  find . -type f | LC_ALL=C sort >"$staging/entries"
  case "$target" in
    *-apple-darwin)
      COPYFILE_DISABLE=1 tar --format=ustar --uid 0 --gid 0 --uname root --gname root -cf "$staging/archive.tar" -T "$staging/entries"
      ;;
    *-unknown-linux-gnu)
      tar --format=ustar --owner=0 --group=0 --numeric-owner -cf "$staging/archive.tar" -T "$staging/entries"
      ;;
  esac
)
gzip -n -c "$staging/archive.tar" >"$output/$archive"
mkdir "$staging/verify"
tar -xzf "$output/$archive" -C "$staging/verify"
sh scripts/smoke-release.sh "$staging/verify/latchrun"
(
  cd "$output"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$archive" >"$archive.sha256"
  else
    shasum -a 256 "$archive" >"$archive.sha256"
  fi
)
printf '%s\n' "$output/$archive"
