#!/usr/bin/env bash
# Payload of the pinned shared Rust workflow. Run from its private source copy.
set -euo pipefail
: "${PF_PLATFORM_DIR:?real platform checkout required}"
: "${PF_SOURCE_SHA:?exact tested source required}"
: "${SOURCE_DATE_EPOCH:?source commit timestamp required}"
test -f "$PF_PLATFORM_DIR/core/caps.py"
test -f "$PF_PLATFORM_DIR/devices/a133/capabilities.toml"
test -f "$PF_PLATFORM_DIR/devices/a523/capabilities.toml"
test -d "$PF_PLATFORM_DIR/skins/a133"
test -z "${RUSTFLAGS:-}" # musl linker comes from the workspace configuration
test -z "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-}"

echo '::group::Lock and offline vendor gates (including negative control)'
bash scripts/check-vendor.sh
bash scripts/test-check-vendor.sh
cargo metadata --offline --locked --format-version 1 > /dev/null
echo '::endgroup::'
echo '::group::Whole workspace with real platform fixtures'
cargo test --locked --workspace --offline --no-fail-fast -- --nocapture
echo '::endgroup::'
echo '::group::Wayland desktop keyboard feature'
cargo test --offline --locked -p pf-framehost-wayland --features keyboard
echo '::endgroup::'
echo '::group::AArch64 musl cross-build, workspace rust-lld (no RUSTFLAGS)'
cargo build --offline --locked --target aarch64-unknown-linux-musl -p pf-input-collect --bin pf-input-collect
cargo build --offline --locked --target aarch64-unknown-linux-musl -p pf-collect-ui --bin pf-collect-ui
cargo build --offline --locked --target aarch64-unknown-linux-musl -p pf-framehost-wayland
echo '::endgroup::'
echo '::group::GNU AArch64 staticlib C SDK and ELF ABI smoke'
bash ctest/run-aarch64.sh
echo '::endgroup::'
echo '::group::Format and whole-workspace clippy (warnings are errors)'
cargo fmt --all -- --check
cargo clippy --locked --workspace --offline --all-targets -- -D warnings
cargo clippy --locked --offline -p pf-framehost-wayland --all-targets --features keyboard -- -D warnings
sccache --show-stats
echo '::endgroup::'
echo "rust_ci=pass source=$PF_SOURCE_SHA platform=$PF_PLATFORM_SHA"
