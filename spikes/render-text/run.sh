#!/bin/sh
set -eu
cd "$(dirname "$0")"
cargo build --release
bin=target/release/pf-render-text-spike
mkdir -p evidence
"$bin" evidence/home-1280x720.png 1280 720 1
"$bin" evidence/home-1024x600.png 1024 600 1
"$bin" evidence/home-1280x720-200.png 1280 720 2
"$bin" bench > evidence/bench-x86_64.txt
"$bin" /tmp/pf-render-a.png 1280 720 1
"$bin" /tmp/pf-render-b.png 1280 720 1
test "$(sha256sum /tmp/pf-render-a.png | cut -d' ' -f1)" = "$(sha256sum /tmp/pf-render-b.png | cut -d' ' -f1)"
sha256sum evidence/*.png > evidence/SHA256SUMS
echo PASS: renders and byte-identical rerun

