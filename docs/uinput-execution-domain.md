# Kernel input-device execution domain

PocketForge code must not create a kernel input device unless it has positively
identified a private `pfvd` guest kernel, the physical DUT, or the transitional
seatless `pf-builder-vm`.  A container is never a boundary: uinput devices live
in its host kernel.

Every creator evaluates this predicate before opening `/dev/uinput`, configuring
the device, or issuing `UI_DEV_CREATE`/`UI_CREATE`:

1. A container is refused, including a container that can read the host device
   tree. Probe errors are refused.
2. The current A133 board identity is the complete compatible set
   `pocketforge,tsp`, `allwinner,sun50i-a133`, `allwinner,sun50i-a100`. In a VM
   this is the `pfvd` guest; on bare metal it is a DUT.
3. The legacy DUT identity is the complete compatible set `allwinner,a133`,
   `arm,sun50iw10p1`. It is accepted only on bare metal.
4. Transitional builder admission requires all of: a VM; the hardened,
   root-owned `/etc/pocketforge/ephemeral-runner.conf` marker; and readable
   logind state proving that every session has an empty `Seat=` property. Any
   nonempty seat refuses regardless of whether `Type=` is `x11`, `wayland`,
   `tty`, `mir`, `web`, `unspecified`, empty, or a future value. The marker is
   opened without following symlinks and the opened file descriptor itself is
   validated, so pathname replacement cannot validate one inode and consume
   another. This exception is removed by `tsp-ihne1.56.5`.
5. Everything else is refused with `reason=unsafe_uinput_domain` and a stable
   `domain=` class. An environment variable, root, a group, a device name, the
   presence of `/dev/uinput`, or VM detection by itself never authorizes use.

Every subprocess probe has a 2.0 second deadline. A missing, failed, or timed
out probe refuses as `domain=probe-error`, with `probe=systemd-detect-virt` or
`probe=loginctl` appended to the reason token. Device-tree and marker read
errors similarly identify `probe=device-tree-compatible` or
`probe=builder-marker`. A nonempty device-tree compatible property must be a
well-formed NUL-terminated string list; malformed or unterminated data refuses.

The pfvd DT is the verified unmodified release DTB passed by
`pfvd/crates/pfvd-core/src/runtime/qemu/command.rs`; its digest is pinned in
`artifacts.rs`. Its source identity is committed in kernel-sunxi-7.x at
`arch/arm64/boot/dts/allwinner/sun50i-a133-pocketforge-tsp.dts:33`. The legacy
DUT DT is selected by platform `devices/a133/profile.toml` and committed in
kernel-sunxi-4.9 at `arch/arm64/boot/dts/sunxi/pocketforge_tsp.dts:7`.

`tests/fixtures/uinput-domain-v1.tsv` is the normative conformance data. Ports
must consume those exact bytes and pin their SHA-256. Fields are tab-separated;
`compatibles` is a semicolon-separated set (compatible strings themselves contain
commas). Probe enums are intentionally closed so
unknown input fails fixture parsing instead of becoming an accidental allow.
The `probe` column is `-` unless the refusal is a typed probe error.

Allowed results use `reason=safe_uinput_domain`. Refused results always use
`reason=unsafe_uinput_domain`.
