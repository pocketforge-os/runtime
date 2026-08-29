#!/bin/sh
set -eu

runtime_dir="$(mktemp -d)"
weston_log="$runtime_dir/weston.log"
cleanup() {
    if [ "${weston_pid:-}" ]; then kill "$weston_pid" 2>/dev/null || true; fi
    rm -rf "$runtime_dir"
}
trap cleanup EXIT INT TERM
chmod 700 "$runtime_dir"
export XDG_RUNTIME_DIR="$runtime_dir"
export WAYLAND_DISPLAY=wayland-pocketforge-test

start_weston() {
    weston --backend=headless-backend.so --socket="$WAYLAND_DISPLAY" --idle-time=0 \
        --log="$weston_log" >/dev/null 2>&1 &
    weston_pid=$!
    i=0
    while [ ! -S "$runtime_dir/$WAYLAND_DISPLAY" ]; do
        i=$((i + 1))
        if [ "$i" -ge 100 ] || ! kill -0 "$weston_pid" 2>/dev/null; then
            cat "$weston_log" >&2
            exit 1
        fi
        sleep 0.05
    done
}

start_weston
cargo run --quiet --locked -p pf-framehost-wayland --example headless_fixture
kill "$weston_pid"
wait "$weston_pid" || true
if cargo run --quiet --locked -p pf-framehost-wayland --example headless_fixture 2>&1; then
    echo "expected compositor-death connection failure" >&2
    exit 1
else
    echo "DISCONNECT typed connection failure"
fi
start_weston
cargo run --quiet --locked -p pf-framehost-wayland --example headless_fixture
echo "RECONNECT ok"

# Exercise compositor loss and recovery on one live WaylandHost. The fixture
# owns its compositor so it can kill and restart it without replacing the host.
cargo run --quiet --locked -p pf-framehost-wayland --example reconnect_fixture
