#!/usr/bin/env bash
# Refresh the committed registry sources after an intentional Cargo.lock update.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

test -f Cargo.lock || { echo "refresh-vendor: Cargo.lock is missing" >&2; exit 1; }
test -z "$(git status --porcelain)" || {
  echo "refresh-vendor: worktree must be clean before refresh" >&2
  exit 1
}

tmp="$(mktemp -d "${TMPDIR:-/tmp}/pocketforge-vendor.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
cargo vendor --locked --versioned-dirs "$tmp/vendor" >/dev/null

# Cargo must resolve through the committed relative source replacement, never an
# absolute path printed by cargo vendor for this temporary destination.
grep -Fq 'replace-with = "vendored-sources"' .cargo/config.toml
grep -Fq 'directory = "vendor"' .cargo/config.toml

rm -rf vendor
mv "$tmp/vendor" vendor
lock_sha="$(sha256sum Cargo.lock | cut -d' ' -f1)"
cargo_version="$(cargo -V | tr -s ' ')"
package_count="$(find vendor -mindepth 2 -maxdepth 2 -name .cargo-checksum.json | wc -l | tr -d ' ')"
{
  printf 'cargo_lock_sha256=%s\n' "$lock_sha"
  printf 'cargo_version=%s\n' "$cargo_version"
  printf 'vendored_packages=%s\n' "$package_count"
} > vendor/.pocketforge-vendor-lock

cargo metadata --offline --locked --format-version 1 >/dev/null
cargo build --offline --locked --workspace
cargo test --offline --locked --workspace --no-fail-fast
echo "refresh-vendor: refreshed $package_count packages"
