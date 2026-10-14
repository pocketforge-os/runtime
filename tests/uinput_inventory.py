#!/usr/bin/env python3
"""Reject unclassified kernel-input creation/exposure occurrences."""

import argparse
from collections import Counter, defaultdict
import hashlib
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "tests/uinput-inventory.txt"
SOURCE_SUFFIXES = {".c", ".h", ".py", ".rs", ".sh", ".toml", ".service", ".yml", ".yaml"}
PATTERN = re.compile(r"/dev/uinput|\bUI_(?:DEV_)?CREATE\b|\bUInput\s*\(|\bVirtualDevice(?:::\w+)?\s*\(|(?:--device|-v|--mount)[^\n]*/dev/input")


def fingerprint(path, line):
    digest = hashlib.sha256(line.strip().encode()).hexdigest()
    return f"{path}\t{digest}"


def tracked():
    paths = []
    for path in ROOT.rglob("*"):
        rel = path.relative_to(ROOT)
        if not path.is_file() or any(part in {".git", "vendor", "target", "build"} for part in rel.parts):
            continue
        paths.append(rel)
    return paths


def occurrences(extra=None):
    found = []
    for rel in tracked():
        if rel.parts[0] == "vendor" or rel.name in {MANIFEST.name, Path(__file__).name}:
            continue
        if rel.suffix not in SOURCE_SUFFIXES and rel.name != "Makefile":
            continue
        try:
            lines = (ROOT / rel).read_text(encoding="utf-8").splitlines()
        except UnicodeDecodeError:
            continue
        for number, line in enumerate(lines, 1):
            if PATTERN.search(line):
                found.append((fingerprint(rel.as_posix(), line), rel.as_posix(), number, line.strip()))
    if extra:
        for path, line in extra:
            found.append((fingerprint(path, line), path, 1, line))
    return found


def load_manifest():
    return Counter(line.split("\t#", 1)[0] for line in MANIFEST.read_text().splitlines()
                   if line and not line.startswith("#"))


def check(extra=None, quiet=False, allowed=None):
    allowed = load_manifest() if allowed is None else allowed
    items = occurrences(extra)
    live = Counter(item[0] for item in items)
    by_key = defaultdict(list)
    for item in items:
        by_key[item[0]].append(item)
    unclassified = live - allowed
    stale = allowed - live
    if not quiet:
        for key, count in unclassified.items():
            for _, path, number, line in by_key[key][-count:]:
                print(f"unclassified uinput occurrence: {path}:{number}: {line}", file=sys.stderr)
        for key, count in stale.items():
            print(f"stale uinput inventory entry ({count}): {key}", file=sys.stderr)
    return not unclassified and not stale


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--emit", action="store_true")
    args = parser.parse_args()
    if args.emit:
        for key, path, number, line in occurrences():
            print(f"{key}\t# {path}:{number} {line}")
        return 0
    known = occurrences()[0]
    if args.self_test and check([(known[1], known[3])], quiet=True):
        print("inventory self-test failed: duplicate live occurrence was accepted", file=sys.stderr)
        return 2
    stale = load_manifest()
    stale[f"seeded/stale.c\t{'0' * 64}"] += 1
    if args.self_test and check(quiet=True, allowed=stale):
        print("inventory self-test failed: stale manifest entry was accepted", file=sys.stderr)
        return 2
    if args.self_test and check([("seeded/violation.c", 'open("/dev/uinput", O_RDWR);')], quiet=True):
        print("inventory self-test failed: seeded violation was accepted", file=sys.stderr)
        return 2
    if not check():
        return 1
    print("uinput-inventory: PASS seeded_violation=red duplicate_live=red "
          "stale_manifest=red clean_tree=green")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
