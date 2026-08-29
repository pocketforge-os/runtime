#!/bin/sh
set -eu

design_root=${1:-${PF_DESIGN_ROOT:-}}
if [ -z "$design_root" ]; then
    echo "usage: $0 /path/to/design" >&2
    exit 2
fi
source_dir=$design_root/theme-quiet-console/package
crate_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

(cd "$source_dir" && sha256sum manifest.json tokens.json motifs/*.svg | LC_ALL=C sort) \
    | diff -u "$crate_dir/vendor/SOURCE.sha256" -
diff -ru --no-dereference "$source_dir" "$crate_dir/vendor/package"
