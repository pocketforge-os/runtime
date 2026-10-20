#!/usr/bin/env bash
# Device-free execution-domain conformance and inventory checks.
set -euo pipefail

bash tests/test_uinput_domain_c.sh
python3 tests/test_uinput_domain_guard.py
python3 tests/test_uinput_inventory_empty_tree.py
python3 tests/test_uinput_inventory_input_safety.py
python3 tests/test_uinput_inventory_keys.py
python3 tests/uinput_inventory.py --self-test
# Plain scan: creator manifest + host-route inventory over the real tree
# (the vendored runner-label map .SOURCE sha pin is enforced here too).
python3 tests/uinput_inventory.py
