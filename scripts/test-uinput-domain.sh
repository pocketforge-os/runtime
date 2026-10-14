#!/usr/bin/env bash
# Device-free execution-domain conformance and inventory checks.
set -euo pipefail

bash tests/test_uinput_domain_c.sh
python3 tests/test_uinput_domain_guard.py
python3 tests/uinput_inventory.py --self-test
