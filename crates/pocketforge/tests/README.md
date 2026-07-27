# Runtime facade tests — where the device truth comes from

**There is no vendored device descriptor in this repo.** The suite reads
`platform/devices/<id>/capabilities.toml` from a `platform` checkout, directly, every run.

Until `tsp-ozbp.16` this directory held `fixtures/{a133,a523}-capabilities.toml` — hand-copied
snapshots of those descriptors. Nothing forced the copies to agree with the originals, and by the
time the gap was audited they had drifted badly: the a133 face-button **labels** were the
pre-correction glyph set, and the a523 **`[[sensors]]` IMU row** was still present months after
platform removed it (SPIKE-0, 2026-07-11, adjudicated the `qmi8658` DT-present but driver-UNBOUND).
Four tests were asserting an IMU the device does not expose, a fifth was asserting analog triggers
platform now describes as binary, and a sixth (`live_probe_demotes_an_unbound_imu_to_hardware_absent`)
would have started passing vacuously. All six were green. That is the two-copies-of-one-truth
defect the parent bead `tsp-ozbp` exists to kill, one level up — so the copy was deleted rather
than policed.

## How resolution works

`pocketforge::test_support` (`crates/pocketforge/src/test_support.rs`) finds the checkout:

1. `$PF_PLATFORM_DIR` — the platform repo **root** (not `devices/`). This is the same variable
   `pf-input-collect` / `pf-collect-ui` use and the one `.github/workflows/runtime-tests.yml`
   sets, so one checkout serves every runtime test that needs platform assets.
2. Otherwise a sibling checkout: `../../../platform`, `../../platform`, `../platform`
   relative to `crates/pocketforge`.

**If neither resolves, the tests PANIC — they do not skip.** A skip would rebuild the failure mode
the copy created: a suite that is green because it asserted nothing. The panic message says
exactly what to clone or set.

```bash
# either
git clone https://github.com/pocketforge-os/platform ../platform
# or
PF_PLATFORM_DIR=/path/to/platform cargo test -p pocketforge -p pf-input-broker
```

## Consequences you should expect

- **A platform descriptor change can turn runtime CI red on an unrelated PR.** That is the
  mechanism working, not a bug — it is the only thing that makes a descriptor correction
  *reach* the code that consumes it. Reconcile the assertion with the new truth; never pin
  the descriptor to keep the suite quiet.
- **Device tests assert what the device IS.** When a capability is not on a shipping device,
  the capability's own code path is exercised through an explicitly SYNTHETIC descriptor
  (`common::imu_descriptor()`, `common::analog_trigger_descriptor()`,
  `common::gnss_descriptor()`), never by pretending a133/a523 has hardware it does not.
  `no_vendored_descriptor_copy_exists` in `taxonomy.rs` keeps the deleted copy from returning.
