# Headless acceptance transcript

Environment: Ubuntu 24.04 x86_64, Weston `13.0.0-4build3` (`weston 13.0.0`),
Rust target `x86_64-unknown-linux-gnu`.

Command:

```text
$ crates/pf-framehost-wayland/tests/headless-weston.sh
CONNECT ok
CONFIGURE SurfaceMetrics { logical_width: 640.0, logical_height: 480.0, scale: 1.0, safe_insets: Insets { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 }, orientation: Landscape }
PRESENT PresentAck { sequence: 1 }
DISCONNECT typed connection failure
CONNECT ok
CONFIGURE SurfaceMetrics { logical_width: 640.0, logical_height: 480.0, scale: 1.0, safe_insets: Insets { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 }, orientation: Landscape }
PRESENT PresentAck { sequence: 1 }
RECONNECT ok
CONNECT ok
CONFIGURE SurfaceMetrics { logical_width: 640.0, logical_height: 480.0, scale: 1.0, safe_insets: Insets { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 }, orientation: Landscape }
PRESENT PresentAck { sequence: 1 }
DISCONNECT SurfaceLost
RECONNECT ok
PRESENT PresentAck { sequence: 1 }
```

The final seven lines come from one live client process. That run uses a real xdg-shell
configure, `wl_shm` present, compositor process death, typed
`PresentFailure::SurfaceLost`, `WaylandHost::reconnect`, a second configure, and a
second present on the same `WaylandHost`. The scene is the same Japanese-label,
non-round-number geometry fixture used by `pf-framehost`'s offscreen/fbdev parity tests.
The expected panic text from the separate fresh-process connection-failure probe is
omitted above; the harness converts its non-zero exit into `DISCONNECT typed connection
failure`.

Arm64 proof from the same checkout:

```text
$ CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc cargo build --locked --target aarch64-unknown-linux-gnu -p pf-framehost-wayland
Finished `dev` profile [unoptimized + debuginfo] target(s)
```
