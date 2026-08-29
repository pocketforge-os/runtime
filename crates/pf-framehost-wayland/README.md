# pf-framehost-wayland

Production Wayland `FrameHost`: a normal xdg-shell toplevel backed by `wl_shm`.
`pf-render` continues to rasterize on the CPU; this crate only converts/presents its
buffers and forwards raster damage with `wl_surface.damage_buffer`.

`WaylandHost::connect()` waits for the first compositor configure. Protocol transport
loss is returned through `FrameHost` as `PresentFailure::SurfaceLost`; callers can use
`WaylandHost::reconnect()` to rebuild the connection and all protocol objects.

Headless smoke test (Ubuntu/Weston):

```sh
crates/pf-framehost-wayland/tests/headless-weston.sh
```

Reproducible arm64 build:

```sh
rustup target add aarch64-unknown-linux-gnu
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --locked --target aarch64-unknown-linux-gnu -p pf-framehost-wayland
```

