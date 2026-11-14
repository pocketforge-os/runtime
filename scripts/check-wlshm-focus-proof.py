#!/usr/bin/env python3
"""Fail closed unless a Rust source file expresses the focus-proof contract."""

from __future__ import annotations

import pathlib
import sys


REQUIRED = (
    '"--socket"',
    "socket.is_absolute()",
    "UnixStream::connect",
    "Connection::from_socket",
    '"wl_compositor"',
    '"wl_shm"',
    '"xdg_wm_base"',
    "create_surface",
    "create_pool",
    "create_buffer",
    "surface.attach",
    "surface.damage_buffer",
    "surface.commit",
    "blocking_dispatch",
    "alive=dispatch",
)
FORBIDDEN = (
    "vulkan",
    "zink",
    "libgl",
    "libegl",
    "xwayland",
)


def check(path: pathlib.Path) -> list[str]:
    source = path.read_text(encoding="utf-8").lower()
    failures = [f"missing={token}" for token in REQUIRED if token.lower() not in source]
    failures.extend(f"forbidden={token}" for token in FORBIDDEN if token in source)
    if source.count("surface.commit") < 2:
        failures.append("missing=initial_and_buffer_commits")
    return failures


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {pathlib.Path(sys.argv[0]).name} SOURCE", file=sys.stderr)
        return 2
    path = pathlib.Path(sys.argv[1])
    failures = check(path)
    if failures:
        print("PF_WLSHM_SOURCE_CONTRACT result=FAIL " + " ".join(failures))
        return 1
    print("PF_WLSHM_SOURCE_CONTRACT result=PASS socket=explicit surface=wl_shm lifecycle=alive")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
