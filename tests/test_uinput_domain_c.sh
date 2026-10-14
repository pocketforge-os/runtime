#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
expected=538fce952f5de6cb901c46c1919aedc44dcd436e753f7786c1a6e5fe21ad2fb9
actual="$(sha256sum "$root/tests/fixtures/uinput-domain-v1.tsv" | awk '{print $1}')"
[[ "$actual" == "$expected" ]]
tmp="$(mktemp -d "${RUNNER_TEMP:-/tmp}/uinput-domain-c.XXXXXX")"
trap 'find "$tmp" -mindepth 1 -delete; rmdir "$tmp"' EXIT
cc -std=c11 -Wall -Wextra -Werror -I"$root" "$root/tests/uinput_domain_c_test.c" -o "$tmp/test"
"$tmp/test" "$root/tests/fixtures/uinput-domain-v1.tsv"
