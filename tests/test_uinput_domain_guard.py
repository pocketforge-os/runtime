#!/usr/bin/env python3
import csv
import hashlib
import os
from pathlib import Path
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "guard"))
import uinput_domain_guard as guard  # noqa: E402

FIXTURE = ROOT / "tests/fixtures/uinput-domain-v1.tsv"
FIXTURE_SHA256 = "538fce952f5de6cb901c46c1919aedc44dcd436e753f7786c1a6e5fe21ad2fb9"


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
            assert (got.probe or "-") == row["probe"], row["case"]
            calls = []
            try:
                guard.guarded_open("mock", 0, probe=probe,
                                   opener=lambda path, flags, mode: calls.append((path, flags, mode)) or 73)
                assert got.allowed, row["case"]
            except guard.UnsafeUinputDomain as error:
                assert not got.allowed, row["case"]
                assert "reason=unsafe_uinput_domain" in str(error)
            assert len(calls) == int(got.allowed), row["case"]

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        sleeper = root / "sleeping-probe"
        sleeper.write_text("#!/bin/sh\nexec /bin/sleep 5\n", encoding="utf-8")
        sleeper.chmod(0o755)
        missing = root / "missing-probe"
        started = time.monotonic()
        assert guard._command_check("--quiet", program=str(sleeper), timeout=0.05) is guard.Check.ERROR
        assert time.monotonic() - started < 1
        assert guard._command_check("--quiet", program=str(missing), timeout=0.05) is guard.Check.ERROR
        assert guard._run_loginctl("list-sessions", program=str(sleeper), timeout=0.05) is None
        assert guard._run_loginctl("list-sessions", program=str(missing), timeout=0.05) is None

        malformed_dt = root / "compatible"
        malformed_dt.write_bytes(b"pocketforge,tsp\0allwinner,sun50i-a133\0allwinner,sun50i-a100")
        assert guard._probe_dt(malformed_dt) == ("error", frozenset())

        marker_link = root / "marker"
        marker_link.symlink_to(root / "missing-target")
        assert guard._probe_marker(marker_link) == "invalid"

    print("uinput-domain-python: PASS fixture_rows=23 opener=mocked "
          "sleeping_probe=refused missing_probe=refused malformed_dt=refused symlink_marker=refused")


if __name__ == "__main__":
    main()
