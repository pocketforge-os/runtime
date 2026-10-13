#!/usr/bin/env python3
"""Generate the keyboardLayout enum data file from xkeyboard-config.

Reads the XKB rules XML (evdev.xml) and emits one `layout` / `layout(variant)`
token per line, sorted and deduplicated, with a provenance header. The output is
committed as crates/pf-prefs/keyboard-layouts.txt and consumed by
crates/pf-prefs/build.rs, which turns it into the closed enum for the
`keyboardLayout` preference. Run inside a container with xkb-data installed:

    python3 tools/gen-keyboard-layouts.py \
        [--xml /usr/share/X11/xkb/rules/evdev.xml] \
        [--xkb-data-version "$(dpkg-query -W -f='${Version}' xkb-data)"] \
        crates/pf-prefs/keyboard-layouts.txt

Layout names containing a comma are multi-name alternatives in the XKB data;
only the first (canonical) name is emitted, matching how the rules file selects
layouts. Tokens that do not match the `layout(variant)` grammar are reported on
stderr and skipped so the enum never carries a token the session wiring cannot
split back into XKB_DEFAULT_LAYOUT / XKB_DEFAULT_VARIANT.
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
import xml.etree.ElementTree as ET

TOKEN_RE = re.compile(r"^[a-z0-9]+(\([a-zA-Z0-9_.+\-]+\))?$")


def collect_tokens(xml_path: str) -> list[str]:
    tree = ET.parse(xml_path)
    tokens: set[str] = set()
    skipped: set[str] = set()

    def add(token: str) -> None:
        if TOKEN_RE.match(token):
            tokens.add(token)
        else:
            skipped.add(token)

    for layout in tree.iter("layout"):
        name_el = layout.find("configItem/name")
        if name_el is None or not (name_el.text or "").strip():
            continue
        names = [n.strip() for n in (name_el.text or "").split(",") if n.strip()]
        if not names:
            continue
        canonical = names[0]
        add(canonical)
        for variant in layout.iter("variant"):
            vname_el = variant.find("configItem/name")
            if vname_el is None or not (vname_el.text or "").strip():
                continue
            add(f"{canonical}({vname_el.text.strip()})")

    for token in sorted(skipped):
        print(f"skipped token outside layout(variant) grammar: {token!r}", file=sys.stderr)
    if not tokens:
        raise SystemExit(f"no layout tokens found in {xml_path}")
    if "us" not in tokens:
        raise SystemExit(f"'us' missing from {xml_path}; refusing to generate")
    return sorted(tokens)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--xml", default="/usr/share/X11/xkb/rules/evdev.xml")
    parser.add_argument(
        "--xkb-data-version",
        default=None,
        help="xkb-data package version to record in the provenance header",
    )
    parser.add_argument("output")
    args = parser.parse_args()

    with open(args.xml, "rb") as f:
        xml_sha256 = hashlib.sha256(f.read()).hexdigest()

    tokens = collect_tokens(args.xml)
    version = args.xkb_data_version
    if version is None:
        import subprocess

        version = subprocess.run(
            ["dpkg-query", "-W", "-f=${Version}", "xkb-data"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()

    lines = [
        "# keyboardLayout enum data - GENERATED FILE, do not edit by hand.",
        "# Source: xkeyboard-config evdev.xml (xkb-data {version}), sha256 {digest}".format(
            version=version, digest=xml_sha256
        ),
        "# Command: python3 tools/gen-keyboard-layouts.py \\",
        "#            --xml {xml} --xkb-data-version {version} <output>".format(
            xml=args.xml, version=version
        ),
        "# One `layout` or `layout(variant)` token per line; crates/pf-prefs/build.rs",
        "# turns this list into the closed enum for the keyboardLayout preference.",
        "",
    ]
    lines.extend(tokens)
    lines.append("")

    with open(args.output, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    print(f"wrote {len(tokens)} tokens to {args.output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
