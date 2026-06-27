/*
 * smoke.c — C-ABI link + behavior smoke test for libpocketforge.
 *
 * Proves a plain C program can link the staticlib, open a session against a real
 * capabilities.toml, and observe the descriptor-honest contract:
 *   - a133 (base Pro): no IMU   -> pf_acquire("imu")      == PF_HARDWARE_ABSENT
 *                      no motor  -> pf_rumble_pulse(...)   == PF_RUMBLE_NOOP_ABSENT
 *                      no GNSS   -> pf_acquire("location") == PF_HARDWARE_ABSENT (DT-unbound,
 *                                   so the descriptor omits it; consent path is tested in Rust)
 *                      entropy ungated                    -> pf_acquire == PF_OK, fill ok
 *
 * Usage: smoke <a133-capabilities.toml>
 */
#include <stdio.h>
#include <string.h>
#include "pocketforge.h"

static int fails = 0;
#define CHECK(cond, msg) do { \
    if (cond) { printf("ok   - %s\n", msg); } \
    else { printf("FAIL - %s\n", msg); fails++; } \
} while (0)

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: smoke <a133-capabilities.toml>\n"); return 2; }

    printf("libpocketforge wire version = %u\n", pf_wire_version());
    CHECK(pf_wire_version() == 1, "wire version is 1");

    PfSession *pf = pf_connect_descriptor(argv[1]);
    CHECK(pf != NULL, "pf_connect_descriptor(a133) succeeds");
    if (!pf) return 1;

    /* input is always present */
    CHECK(pf_is_present(pf, "input") == 1, "input present");

    /* a133 has no IMU -> hardware-absent, NOT a crash */
    PfPresence imu = pf_has_capability(pf, "imu");
    CHECK(imu.api == 1 && imu.hardware == 0, "imu: api present, hardware absent (a133)");
    int imu_acq = pf_acquire(pf, "imu");
    printf("     pf_acquire(imu) = %d (%s)\n", imu_acq, pf_strerror(imu_acq));
    CHECK(imu_acq == PF_HARDWARE_ABSENT, "acquire(imu) == HARDWARE_ABSENT on a133");

    /* rumble cosmetic no-op tier: acquire OK, pulse is a typed no-op (no motor on a133) */
    CHECK(pf_acquire(pf, "vibration") == PF_OK, "acquire(vibration) == OK (cosmetic tier)");
    int r = pf_rumble_pulse(pf, 40);
    CHECK(r == PF_RUMBLE_NOOP_ABSENT, "rumble pulse == NOOP_ABSENT on a133");

    /* a133 has no GNSS (DT-unbound; descriptor omits it) -> hardware-absent, not consent */
    CHECK(pf_query(pf, "location") == PF_DENIED, "query(location) == DENIED (a133 no GNSS)");
    CHECK(pf_acquire(pf, "location") == PF_HARDWARE_ABSENT, "acquire(location) == HARDWARE_ABSENT");

    /* entropy ungated */
    CHECK(pf_acquire(pf, "entropy") == PF_OK, "acquire(entropy) == OK (ungated)");
    unsigned char buf[16] = {0};
    CHECK(pf_entropy_fill(pf, buf, sizeof buf) == 0, "entropy fill succeeds");

    pf_free(pf);
    printf(fails ? "\nSMOKE FAILED (%d)\n" : "\nSMOKE OK\n", fails);
    return fails ? 1 : 0;
}
