#!/usr/bin/env bash
set -euo pipefail

: "${PF_SOURCE_SHA:?exact source commit is required}"
: "${PF_IMAGE:?pinned CI image identity is required}"
: "${SOURCE_DATE_EPOCH:?source commit timestamp is required}"
[[ "$PF_SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]]
[[ "$PF_IMAGE" =~ @sha256:[0-9a-f]{64}$ ]]
test "$(git rev-parse HEAD)" = "$PF_SOURCE_SHA"
test -z "${RUSTFLAGS:-}"
test -z "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-}"

artifact_root=${PF_WLSHM_ARTIFACT_ROOT:-.ci-artifacts/wlshm-focus-proof}
target_root=${PF_WLSHM_TARGET_ROOT:-.ci-target-wlshm-focus-proof}
mkdir -p "$artifact_root"
test -z "$(find "$artifact_root" -mindepth 1 -maxdepth 1 -print -quit)"

CARGO_TARGET_DIR="$target_root" cargo build --offline --locked --release \
  --target aarch64-unknown-linux-musl \
  -p pf-framehost-wayland --bin pf-wlshm-focus-proof
install -m0755 \
  "$target_root/aarch64-unknown-linux-musl/release/pf-wlshm-focus-proof" \
  "$artifact_root/pf-wlshm-focus-proof"

binary="$artifact_root/pf-wlshm-focus-proof"
machine=$(readelf -h "$binary" | sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p')
test "$machine" = AArch64
if readelf -d "$binary" | grep -q '(NEEDED)'; then
  echo 'reason=unexpected_dynamic_dependency' >&2
  readelf -d "$binary" >&2
  exit 1
fi
if strings -a "$binary" | grep -Eiq 'libvulkan|zink|libgl\.so|libegl'; then
  echo 'reason=forbidden_graphics_dependency_string' >&2
  exit 1
fi

binary_sha=$(sha256sum "$binary" | cut -d' ' -f1)
binary_bytes=$(stat -c %s "$binary")
{
  echo 'PF_WLSHM_FOCUS_PROOF_ARTIFACT_V1'
  echo "source_commit=$PF_SOURCE_SHA"
  echo "source_date_epoch=$SOURCE_DATE_EPOCH"
  echo "ci_image=$PF_IMAGE"
  echo "rustc=$(rustc --version)"
  echo 'cargo=release,offline,locked,target=aarch64-unknown-linux-musl,package=pf-framehost-wayland,bin=pf-wlshm-focus-proof'
  echo "binary_sha256=$binary_sha"
  echo "binary_bytes=$binary_bytes"
  echo "elf_machine=$machine"
  echo 'elf_needed=none'
  echo 'forbidden_dependency_strings=absent(libvulkan,zink,libgl.so,libegl)'
} >"$artifact_root/BUILD-RECEIPT.txt"
sha256sum "$artifact_root/BUILD-RECEIPT.txt" "$binary" >"$artifact_root/SHA256SUMS"
sha256sum -c "$artifact_root/SHA256SUMS"
cat "$artifact_root/BUILD-RECEIPT.txt"
