#!/usr/bin/env python3
"""Cross-check pf-framehost's fbdev orientation against the image boot animator.

The boot animator (image apps/pocketforge-boot-animator) paints the first
frame, and pf-shell's pf-framehost paints the next one. If they read the
connector "panel orientation" differently, the handoff flips the panel. Both
claim the kernel's meaning of the property (see `panel_orientation_rotation`
in crates/pf-framehost/src/lib.rs).

This script runs the real animator binary through the image's own hermetic
harness (tests/fakefb.c, LD_PRELOAD). For each of the four orientation values
it paints a coded card: every scene pixel carries its own (u, v). It then
hashes the page the animator presents, and requires:

  1. the page equals the image test's kernel-formula page (`expected_page`),
     so this run agrees with the animator's own goldens; and
  2. the sha256 equals the row for that value in the ANIMATOR_FIRST_FRAME
     table of crates/pf-framehost/src/lib.rs. The pf-framehost unit test
     `animator_first_frame_pages_match_framehost` presents the same card and
     asserts that same hash.

Together, the animator at the given image checkout and pf-framehost at this
checkout place every pixel identically for all four values.

Run it whenever either side changes:

    scripts/check-framehost-animator-orientation.py --image <image checkout>
    scripts/check-framehost-animator-orientation.py --image <dir> --print   # emit table rows

Needs python3, a host C compiler (cc) and the image checkout. No device.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
RUNTIME = os.path.dirname(HERE)
LIB_RS = os.path.join(RUNTIME, "crates", "pf-framehost", "src", "lib.rs")
ROW = re.compile(r'\(\s*"([A-Za-z ]+)",\s*(\d+),\s*(\d+),\s*(\d+),\s*"([0-9a-f]{64})",?\s*\)')


def framehost_table():
    with open(LIB_RS, encoding="utf-8") as fh:
        text = fh.read()
    start = text.index("const ANIMATOR_FIRST_FRAME:")
    block = text[start:text.index("];", start)]
    rows = {}
    for name, xres, yres, stride, digest in ROW.findall(block):
        rows[name] = (int(xres), int(yres), int(stride), digest)
    if len(rows) != 4:
        sys.exit(f"FAIL: expected 4 ANIMATOR_FIRST_FRAME rows in {LIB_RS}, found {len(rows)}")
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--image", required=True, help="image repository checkout")
    ap.add_argument("--print", action="store_true", help="print table rows instead of comparing")
    args = ap.parse_args()

    app = os.path.join(os.path.abspath(args.image), "apps", "pocketforge-boot-animator")
    main_c = os.path.join(app, "src", "main.c")
    sys.path.insert(0, os.path.join(app, "tests"))
    sys.dont_write_bytecode = True
    import test_animator as ta  # noqa: E402  (the image's own harness)

    with open(main_c, "rb") as fh:
        main_sha = hashlib.sha256(fh.read()).hexdigest()
    print(f"animator main.c sha256={main_sha}")

    work = tempfile.mkdtemp(prefix="pf-fh-anim-")
    try:
        return run(args, ta, app, main_c, work)
    finally:
        shutil.rmtree(work, ignore_errors=True)


def run(args, ta, app, main_c, work):
    ctx = ta.Ctx(work)
    cc = os.environ.get("CC", "cc")
    src = os.path.join(app, "src")
    subprocess.run([cc, *ta.CFLAGS, "-I", src, "-o", ctx.bin_new, main_c, "-lm"], check=True)
    subprocess.run([cc, "-O2", "-Wall", "-Wextra", "-fPIC", "-shared", "-o", ctx.shim,
                    os.path.join(app, "tests", "fakefb.c"), "-ldl"], check=True)
    os.makedirs(ctx.card_dir)
    with open(os.path.join(ctx.card_dir, "frame-000.png"), "wb") as fh:
        fh.write(ta.crop_frames.encode_png(ta.SCENE_W, ta.SCENE_H,
                                           ta.card_rows(0, 0, ta.SCENE_W, ta.SCENE_H, 0), level=1))

    table = None if args.print else framehost_table()
    failed = False
    for prop, geom in ta.ORIENT_GEOM.items():
        xres, yres, _, _, stride = geom
        r = ta.run_animator(ctx, ctx.bin_new, geom=geom, fb_id=ta.DRM_ID, drm=f"prop:{prop}",
                            fbcon=(True, "0"), frames=ctx.card_dir, args=("--first-frame",))
        if r.rc != 0:
            sys.exit(f"FAIL {prop}: animator exit {r.rc}: {r.stderr}")
        page = r.fb[:stride * yres]
        if page != ta.expected_page(prop, stride):
            sys.exit(f"FAIL {prop}: animator page differs from the image test's kernel goldens")
        digest = hashlib.sha256(page).hexdigest()
        if args.print:
            print(f'    ("{prop}", {xres}, {yres}, {stride}, "{digest}"),  # then cargo fmt')
            continue
        want = table[prop]
        if want != (xres, yres, stride, digest):
            failed = True
            print(f"FAIL {prop}: animator {xres}x{yres} stride {stride} sha256={digest}; "
                  f"pf-framehost table {want}")
        else:
            print(f"PASS {prop}: animator page sha256={digest} == pf-framehost table")
    if not args.print:
        print("framehost_animator_orientation=" + ("FAIL" if failed else "PASS"))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
