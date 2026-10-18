# Offscreen focus-move benchmark

The ignored `focus_benchmark` integration test measures the complete retained focus-only path:
`pf-menu` input, `MenuSceneCache` update, layout resolution, and software rasterization. It is
hermetic and offscreen; it creates no window and injects no host input.

## Laptop evidence (2026-10-18)

- Surface: 1280x720 at scale 1, six visible rows, dusk theme
- Samples: 240 focus moves after one warm-up frame
- Result: p50 11.432 ms, p95 12.706 ms, max 14.849 ms
- Maximum damage: 415,360 pixels (45.1% of the 921,600-pixel surface), never full-surface
- Toolchain: pinned `pocketforge/ci-rust` image, Rust 1.98.1, release profile
- Private evidence binary SHA-256 before/after: `31c447d3a5b39cf9363909f22f6140835dd8d7421ba4e6e6726362396502580f`

Run it explicitly with:

```sh
cargo test --release -p pf-menu-scene --test focus_benchmark -- --ignored --nocapture
```

This laptop result is an implementation guard, not the device acceptance measurement. The named
device p95 focus-move capture remains for gpu-14's later device window.
