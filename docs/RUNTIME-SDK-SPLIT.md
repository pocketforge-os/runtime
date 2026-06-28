# Runtime / SDK split — the Flatpak-style seam (`tsp-e1b.5`, hands off to E8)

> The hook E8 (`infra-107`) builds packaging + the dev-SDK + distribution on top of. **E2 defines
> the contract; E8 distributes it.** This doc fixes the seam: what is Platform vs SDK, how an app
> pins a per-SoC **family**, where the E2/E8 boundary lies, and the provenance gap it inherits.

## 1. Platform vs SDK (Flatpak's runtime/SDK model)

Like Flatpak's `org.freedesktop.Platform` (runtime) vs `…Sdk` (build-time), PocketForge splits:

| | **Platform** (on device) | **SDK** (build-time only) |
|---|---|---|
| what | the enforcing broker (`pf-broker`) + `libpocketforge.so` + the v0 backends + the device's `capabilities.toml` | `include/pocketforge.h` + the `abi/libpocketforge.v1.abi` contract + `wire/WIRE-PROTOCOL.md` + the target/family definition |
| who ships it | the PocketForge image (the supervisor launches the broker) | pinned by the app at build time (E8 packages it) |
| stability | the frozen v1 ABI/wire (`STABILITY.md`) | the same frozen contract — the SDK *is* the contract headers + version |

An app links the SDK (headers + a stub or the soname), and at runtime finds the Platform's
`libpocketforge.so.1` + the broker socket. Because the ABI/wire are frozen (`STABILITY.md`), a
binary built against SDK v1 runs on any Platform that advertises the v1 contract — the "survives
the runtime fork" property, now as a *distribution* guarantee.

## 2. An app pins a per-SoC FAMILY, not a device (R-D)

The two starter devices are **divergent SoCs**, not variants — the runtime contract is therefore
**per-SoC-family** (≥2 tuples today):

| family id | kernel | GPU | display/SDL backend |
|-----------|--------|-----|---------------------|
| `pocketforge/sun50i-a133` | 4.9 | PowerVR | `sunxifb` (fbdev) SDL |
| `pocketforge/sun55i-a523` | 5.15 | Mali | `kmsdrm` SDL |

An app declares (in its `app.toml`, alongside the `use=[]` authority graph `.3` validates):

```toml
[runtime]
family  = "pocketforge/sun50i-a133"   # the SoC family this build targets
abi     = "1"                          # the frozen libpocketforge/PFW1 contract version
```

The **capability descriptor stays device-level** (`a133` vs `a523` differ by data — `.4`'s zero
per-device-code rule), but the **binary ABI target is family-level** (different kernel/GPU/SDL ⇒
different build). One app source → one build per family it supports; the capability *facade* makes
the within-family device delta invisible. A Platform accepts an app iff
`app.runtime.family == platform.family && app.runtime.abi` is offered.

## 3. The E2 / E8 boundary (decided)

To avoid duplicating E8's packaging:

- **E2 owns (this repo):** the frozen ABI header + `abi/` golden + the wire spec + `STABILITY.md`
  (the versioning/compat/deprecation policy + guards) + the `[runtime] family/abi` vocabulary +
  the family id registry above. This is the *named contract*.
- **E8 owns (`infra-107`):** turning that contract into artifacts — the OCI/Flatpak-style runtime
  image build, the dev-SDK tarball/sysroot, signing + distribution (IPFS/`vendor-manifest`), and
  the `pf-sdk`-style build tooling. E8 consumes the family ids + ABI version + headers verbatim.
- **Shared seam:** the `[runtime]` table schema is defined here and *validated by the broker/
  supervisor at launch* (next to `.3`'s `use=[]` check); E8 only needs to stamp the right
  `family/abi` into the package metadata. When E8 is filed, confirm it adopts this schema rather
  than inventing its own.

## 4. Inherited provenance gap (named, not papered over — R8)

The contract is frozen; its **build is not yet reproducible-from-clean**. The Platform image + the
SDK sysroot are produced today by the interim `sync-build-sources.sh` rsync-of-working-trees +
`make build-image LOCAL_BLOBS=…` flow — not a clean-room build from committed refs. So an app
author cannot yet rebuild a bit-identical SDK/Platform from sources alone; the *interface* is
pinned, the *provenance* is not. Tracked in **`tsp-cv7.4.13`** (provenance gap) and
**`tsp-cv7.6`** / **`tsp-iby`** (one-command container-multistage reproducible build from pinned
git refs + IPFS-fetched blobs). E8 MUST NOT advertise the SDK as reproducible until those land.
See `STABILITY.md` §5.
