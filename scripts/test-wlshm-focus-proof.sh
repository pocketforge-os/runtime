#!/usr/bin/env bash
set -euo pipefail

red_source=vendor/wayland-client-0.31.11/examples/simple_window.rs
green_source=crates/pf-framehost-wayland/src/bin/pf-wlshm-focus-proof.rs

set +e
red_output=$(python3 scripts/check-wlshm-focus-proof.py "$red_source" 2>&1)
red_status=$?
set -e
printf '%s\n' "$red_output"
test "$red_status" -eq 1
python3 scripts/check-wlshm-focus-proof.py "$green_source"
