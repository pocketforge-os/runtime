#!/usr/bin/env python3
"""The scanner's --self-test must pass on a tree with zero creator occurrences.

Ports vendor tests/uinput_inventory.py into repositories that may carry no
live creator occurrence at all (a freshly vendored launcher, for example).
seed_creator_self_test used to index occurrences()[0] unconditionally for the
duplicate-live control and crashed with IndexError on such trees, so the
vendored self-test could never go green there.

This test builds the minimal vendored layout in a private temporary tree
(scanner, canonical fixtures, empty creator manifest), runs --self-test in it
and requires the full green run: the four creator controls, every host-route
case, and the plain scan of the (finding-free) tree.
"""

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

VENDORED = (
    "tests/uinput_inventory.py",
    "tests/fixtures/host-route-policy-v1.json",
    "tests/fixtures/host-route-cases-v1.jsonl",
    "tests/fixtures/input-runner-host-map-v1.json",
    "tests/fixtures/input-runner-host-map-v1.schema.json",
)


class EmptyTreeSelfTest(unittest.TestCase):
    def test_self_test_passes_with_zero_live_occurrences(self):
        with tempfile.TemporaryDirectory(prefix="inventory-empty-tree.") as tmp:
            root = Path(tmp)
            for rel in VENDORED:
                dest = root / rel
                dest.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / rel, dest)
            (root / "tests" / "uinput-inventory.txt").write_text(
                "# uinput inventory (empty port tree)\n", encoding="utf-8")
            proc = subprocess.run(
                [sys.executable, "tests/uinput_inventory.py", "--self-test"],
                cwd=root, text=True, capture_output=True)
            self.assertEqual(
                proc.returncode, 0,
                f"--self-test failed on the empty tree\n"
                f"stdout tail:\n{proc.stdout[-2000:]}\n"
                f"stderr tail:\n{proc.stderr[-2000:]}")
            self.assertIn("uinput-inventory: PASS seeded_violation=red "
                          "duplicate_live=red stale_manifest=red "
                          "clean_tree=green", proc.stdout)
            self.assertIn("all_ok=True", proc.stdout)
            self.assertIn("uinput-inventory self-test: host_route=green "
                          "runs_on=green", proc.stdout)


if __name__ == "__main__":
    unittest.main()
