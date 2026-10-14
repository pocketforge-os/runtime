#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
expected=6f58e75f72b48a3e426417a55056b688b9c1e4cba2cf7a9e92fea686e47a050d
actual="$(sha256sum "$root/tests/fixtures/uinput-domain-v1.tsv" | awk '{print $1}')"
[[ "$actual" == "$expected" ]]
tmp="$(mktemp -d "${RUNNER_TEMP:-/tmp}/uinput-domain-c.XXXXXX")"
trap 'find "$tmp" -mindepth 1 -delete; rmdir "$tmp"' EXIT
cc -std=c11 -Wall -Wextra -Werror -I"$root" "$root/tests/uinput_domain_c_test.c" -o "$tmp/test"
"$tmp/test" "$root/tests/fixtures/uinput-domain-v1.tsv"
