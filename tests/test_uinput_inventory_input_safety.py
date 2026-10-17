#!/usr/bin/env python3
"""Repository inputs must be regular, bounded, and opened without symlinks."""

import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
VENDORED = (
    "tests/uinput_inventory.py",
    "tests/fixtures/host-route-policy-v1.json",
    "tests/fixtures/host-route-cases-v1.jsonl",
    "tests/fixtures/input-runner-host-map-v1.json",
    "tests/fixtures/input-runner-host-map-v1.schema.json",
)
OVERSIZED_BYTES = 4 * 1024 * 1024 + 1


def load_inventory_module():
    spec = importlib.util.spec_from_file_location(
        "uinput_inventory_input_safety", ROOT / "tests/uinput_inventory.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RepositoryInputSafetyTest(unittest.TestCase):
    def make_tree(self, tmp):
        root = Path(tmp)
        for rel in VENDORED:
            dest = root / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / rel, dest)
        (root / "tests" / "uinput-inventory.txt").write_text(
            "# uinput inventory (input-safety test)\n", encoding="utf-8")
        return root

    def run_scan(self, root, *args):
        return subprocess.run(
            [sys.executable, "tests/uinput_inventory.py", *args],
            cwd=root, text=True, capture_output=True, timeout=10)

    def assert_refused(self, root, reason):
        proc = self.run_scan(root)
        self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(f"reason={reason}", proc.stderr)

    def test_normal_regular_file_passes(self):
        with tempfile.TemporaryDirectory(prefix="inventory-input-normal.") as tmp:
            root = self.make_tree(tmp)
            (root / "normal.py").write_text("value = 1\n", encoding="utf-8")
            proc = self.run_scan(root)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_symlink_is_refused_without_following_it(self):
        with tempfile.TemporaryDirectory(prefix="inventory-input-symlink.") as tmp:
            root = self.make_tree(tmp)
            (root / "linked.py").symlink_to("/dev/zero")
            self.assert_refused(root, "repo_input_symlink")

    def test_symlink_to_oversized_sparse_file_is_refused(self):
        with tempfile.TemporaryDirectory(prefix="inventory-input-sparse-link.") as tmp:
            root = self.make_tree(tmp)
            target = root / "large-sparse"
            with target.open("wb") as stream:
                stream.truncate(OVERSIZED_BYTES)
            (root / "linked.py").symlink_to(target.name)
            self.assert_refused(root, "repo_input_symlink")

    def test_oversized_regular_file_is_refused_before_reading(self):
        with tempfile.TemporaryDirectory(prefix="inventory-input-oversized.") as tmp:
            root = self.make_tree(tmp)
            with (root / "oversized.py").open("wb") as stream:
                stream.truncate(OVERSIZED_BYTES)
            self.assert_refused(root, "repo_input_oversized")

    @unittest.skipUnless(hasattr(os, "mkfifo"), "FIFO creation is unavailable")
    def test_fifo_is_refused_without_opening_it(self):
        with tempfile.TemporaryDirectory(prefix="inventory-input-fifo.") as tmp:
            root = self.make_tree(tmp)
            os.mkfifo(root / "blocked.py")
            self.assert_refused(root, "repo_input_non_regular")

    def test_same_size_in_place_rewrite_during_read_is_refused(self):
        with tempfile.TemporaryDirectory(prefix="inventory-input-rewrite.") as tmp:
            root = Path(tmp)
            target = root / "mutable.py"
            target.write_bytes(b"old contents\n")
            inventory = load_inventory_module()
            original_read = os.read
            mutated = False

            def rewrite_after_first_read(descriptor, size):
                nonlocal mutated
                data = original_read(descriptor, size)
                if data and not mutated:
                    mutated = True
                    before = target.stat()
                    target.write_bytes(b"new contents\n")
                    os.utime(target, ns=(before.st_atime_ns, before.st_mtime_ns + 1_000_000_000))
                return data

            with mock.patch.object(inventory.os, "read", side_effect=rewrite_after_first_read):
                with self.assertRaises(inventory.RepoInputError) as raised:
                    inventory.read_repo_bytes(target, root=root)
            self.assertEqual(raised.exception.reason, "repo_input_changed")

    def test_self_test_carries_all_input_controls(self):
        with tempfile.TemporaryDirectory(prefix="inventory-input-self-test.") as tmp:
            root = self.make_tree(tmp)
            proc = self.run_scan(root, "--self-test")
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn(
                "repo_inputs: symlink=red sparse_symlink=red oversized=red "
                "fifo=red normal=green",
                proc.stdout)


if __name__ == "__main__":
    unittest.main()
