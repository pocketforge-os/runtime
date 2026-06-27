# Descriptor test fixtures

`a133-capabilities.toml` / `a523-capabilities.toml` are **verbatim copies** of the E1
(`tsp-9sx`) device capability descriptors from the `platform` repo
(`platform/devices/<id>/capabilities.toml`). They are the off-hardware test oracle for the
runtime facade.

**Provenance / drift guard.** These are a vendored snapshot. The authoritative source is the
`platform` repo (read-only from here). The `fixtures_track_platform` integration test
re-checks them against the live platform descriptors when `PF_PLATFORM_DESCRIPTORS` points at
`platform/devices` (e.g. in CI), so a drift is caught rather than silently tolerated.

> Note (2026-06-27): at the time of vendoring, E1 (`tsp-9sx`) is `in_progress` and its
> `capabilities.toml` files were not yet merged to `platform` `main`. If E1 changes the
> descriptor before it lands, refresh these fixtures and re-run the suite.
