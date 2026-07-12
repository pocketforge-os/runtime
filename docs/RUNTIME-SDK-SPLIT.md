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

| canonical family id | alias (this doc's original draft) | kernel | GPU | display/SDL backend |
|---------------------|-----------------------------------|--------|-----|---------------------|
| `pocketforge/a133-powervr` | `pocketforge/sun50i-a133` | 4.9 | PowerVR | `sunxifb` (fbdev) SDL — **owned** (`libsdl3-sunxifb`) |
| `pocketforge/a523-mali` | `pocketforge/sun55i-a523` | 5.15 | Mali | `kmsdrm` SDL — **not yet an owned fork** (a133-only lib today) |

> **Family-id reconciliation (E8, `tsp-ziac.1`).** This doc first drafted **SoC-only** ids
> (`sun50i-a133` / `sun55i-a523`) and said, in §3 below, *"When E8 is filed, confirm it adopts
> this schema rather than inventing its own."* E8 adopts the **schema** (the `[runtime]
> family/abi` vocabulary) verbatim and reconciles the **ids** to the **GPU-IP-bearing** form
> (`a133-powervr` / `a523-mali`) — the SDL-backend split (PowerVR fbdev vs Mali kmsdrm) is the
> ABI-relevant divergence, so naming the GPU IP is the honest key. The SoC-only ids remain
> **accepted aliases** (an app that pinned the draft name still resolves). The **canonical
> registry + the derived `{kernel,GPU,SDL}` SHA-set view** live in the `platform` repo:
> `abi/families.toml`, `abi/platform-abi.json`, `docs/PLATFORM-ABI-CONTRACT.md` (`pf abi …`).

An app declares (in its `app.toml`, alongside the `use=[]` authority graph the broker validates):

```toml
[runtime]
family           = "pocketforge/a133-powervr"   # the SoC family this build targets (canonical id)
abi              = "1"                            # the frozen libpocketforge/PFW1 contract version
platform-version = "1"                           # (E8) the frozen substrate SHA-set this build pins
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

## 4. Inherited provenance gap — now stated PER FAMILY (named, not papered over — R8)

The contract is frozen; provenance is **per-family**, not a single blanket claim (E8 correction,
`tsp-ziac.1` — the old "interim `sync-build-sources.sh` rsync-of-working-trees" flow described
here is **RETIRED**, replaced by the hermetic `pf build` from committed `platform.lock` refs; see
the `mission-control` repo's `.claude/rules/provenance.md`):

- **`pocketforge/a133-powervr` — SHA-pinned AND reproducible-from-clean.** The a133 image builds
  hermetically from committed refs via `pf build` (`tsp-1dl.4.5` closed; cross-host bit-identical
  `tsp-cv7.6.1` closed). Its `{kernel, GPU, SDL}` set is fully owned + pinned.
- **`pocketforge/a523-mali` — SHA-pinned, NOT yet reproducible.** Kernel + GPU-KM are lock-pinned,
  but the a523 image build is not yet hermetic (`tsp-jet`, open) and blob→IPFS distribution is
  pending (`tsp-iby`, open); its SDL backend is not yet an owned fork (a523 ships no
  `libsdl3-sunxifb` today). **E8 MUST NOT advertise the a523 SDK as reproducible until `tsp-jet`
  lands.**

Neither claims a bit-for-bit *app* build (that is packaging's job). The **authoritative,
machine-checked** per-family provenance posture lives in the `platform` repo's
`abi/platform-abi.json` (`reproducible` + `lock_state`) and `docs/PLATFORM-ABI-CONTRACT.md` §5.
See `STABILITY.md` §5.
