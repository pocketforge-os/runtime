#!/usr/bin/env python3
import csv
import hashlib
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "guard"))
import uinput_domain_guard as guard  # noqa: E402

FIXTURE = ROOT / "tests/fixtures/uinput-domain-v1.tsv"
FIXTURE_SHA256 = "6f58e75f72b48a3e426417a55056b688b9c1e4cba2cf7a9e92fea686e47a050d"


def main():
    raw = FIXTURE.read_bytes()
    assert hashlib.sha256(raw).hexdigest() == FIXTURE_SHA256
    with FIXTURE.open(newline="", encoding="utf-8") as stream:
        for row in csv.DictReader(stream, delimiter="\t"):
            probe = guard.Probe(
                guard.Check(row["container"]), guard.Check(row["vm"]), row["dt"],
                frozenset() if row["compatibles"] == "-" else frozenset(row["compatibles"].split(";")),
                row["marker"], row["logind"],
            )
            got = guard.evaluate(probe)
            assert got.allowed == (row["verdict"] == "allow"), row["case"]
            assert got.reason == row["reason"], row["case"]
            assert got.domain.value == row["domain"], row["case"]
            calls = []
            try:
                guard.guarded_open("mock", 0, probe=probe,
                                   opener=lambda path, flags, mode: calls.append((path, flags, mode)) or 73)
                assert got.allowed, row["case"]
            except guard.UnsafeUinputDomain as error:
                assert not got.allowed, row["case"]
                assert "reason=unsafe_uinput_domain" in str(error)
            assert len(calls) == int(got.allowed), row["case"]
    print("uinput-domain-python: PASS fixture_rows=18 opener=mocked")


if __name__ == "__main__":
    main()
