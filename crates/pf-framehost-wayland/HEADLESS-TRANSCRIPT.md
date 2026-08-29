# Headless acceptance transcript

Environment: Ubuntu 24.04 x86_64, Weston `13.0.0-4build3` (`weston 13.0.0`),
Rust target `x86_64-unknown-linux-gnu`.

Command:

```text
$ cargo run --locked -p pf-framehost-wayland --example reconnect_fixture
CONNECT ok
CONFIGURE SurfaceMetrics { logical_width: 640.0, logical_height: 480.0, scale: 1.0, safe_insets: Insets { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 }, orientation: Landscape }
PRESENT PresentAck { sequence: 1 }
DISCONNECT SurfaceLost
RECONNECT ok
PRESENT PresentAck { sequence: 1 }
```

This run uses a real xdg-shell configure, `wl_shm` present, compositor process death,
typed `PresentFailure::SurfaceLost`, `WaylandHost::reconnect`, a second configure, and
a second present in the same client process. The scene is the same Japanese-label,
non-round-number geometry fixture used by `pf-framehost`'s offscreen/fbdev parity tests.

Arm64 proof from the same checkout:

```text
$ CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc cargo build --locked --target aarch64-unknown-linux-gnu -p pf-framehost-wayland
Finished `dev` profile [unoptimized + debuginfo] target(s)
```
