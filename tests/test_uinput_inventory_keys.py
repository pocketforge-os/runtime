#!/usr/bin/env python3
"""Inventory keys must not depend on where an occurrence sits in its file.

Host-route rows were keyed on the finding text, and malformed-file findings
carry the line number in that text ("unbalanced single quote at line 1641").
Any unrelated edit above the occurrence then staled the manifest row and
turned the inventory red on main (pocketforge-automation#1610; upstreamed
from pocketforge-automation#1617, tsp-mc9m.41.1018.15.1). These controls replay the real scanner in a private tree: moving the occurrence
keeps its row; changing what the finding says still does not.
"""

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))

import uinput_inventory as inventory  # noqa: E402

SHELL = "#!/bin/sh\necho 'never closed\n"
ROW = {
    "path": "scripts/lexer-false-positive.sh", "rule": "dynamic_route",
    "class": "product-reference",
    "text": "unbalanced single quote at line 3",
}


def failures(content, row=ROW):
    found, unused_report, unused_stats = inventory.replay_case({
        "files": {ROW["path"]: content}, "inventory": [row],
    }, ROOT)
    return [f"{item.rule}/{item.detail}" for item in found]


class InventoryKeyTests(unittest.TestCase):
    def test_classified_row_matches_in_place(self):
        self.assertEqual(failures(SHELL), [])

    def test_moving_lines_does_not_stale_a_row(self):
        moved = SHELL.replace("\n", "\n\n\n\n", 1)
        self.assertEqual(failures(moved), [])

    def test_a_different_finding_is_still_unclassified(self):
        changed = "#!/bin/sh\necho \"never closed\n"
        self.assertEqual(
            sorted(failures(changed)),
            ["dynamic_route/stale_inventory", "dynamic_route/unclassified"])

    def test_human_output_keeps_the_line(self):
        finding = inventory.Finding(
            "dynamic_route", "malformed_file", ROW["path"], 1641,
            "unbalanced single quote at line 1641")
        moved = inventory.Finding(
            "dynamic_route", "malformed_file", ROW["path"], 1631,
            "unbalanced single quote at line 1631")
        self.assertEqual(finding.key, moved.key)
        self.assertIn("line=1641", finding.render())
        self.assertIn("at line 1641", finding.render())


if __name__ == "__main__":
    unittest.main()
