/*
 * pocketforge.h — the C ABI for the PocketForge capability facade (libpocketforge).
 *
 * Hand-maintained to match crates/libpocketforge/src/lib.rs. Any-language OCI apps link
 * libpocketforge.{so,a} and call these. The numeric enums are FROZEN as part of the public
 * contract (tsp-e1b.5) and equal the PFW1 wire values (wire/WIRE-PROTOCOL.md).
 *
 * Memory: pf_connect*() returns an owning PfSession* (NULL on error); free it once with
 * pf_free(). `name` args are borrowed NUL-terminated UTF-8.
 */
#ifndef POCKETFORGE_H
#define POCKETFORGE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque capability session. */
typedef struct PfSession PfSession;

/* Two-stage capability detection (API present vs hardware present). */
typedef struct {
    int api;       /* 1 if the capability type exists in this build */
    int hardware;  /* 1 if the descriptor + probe back it on this device */
} PfPresence;

/* pf_acquire status codes (== PFW1 Status). */
#define PF_OK              0
#define PF_UNSUPPORTED     1
#define PF_POLICY_BLOCKED  2
#define PF_CONSENT_DENIED  3
#define PF_HARDWARE_ABSENT 4

/* pf_query permission codes (== PFW1 Permission). */
#define PF_GRANTED 0
#define PF_DENIED  1
#define PF_PROMPT  2

/* pf_rumble_pulse status codes (== PFW1 RumbleStatus). */
#define PF_RUMBLE_FIRED           0
#define PF_RUMBLE_NOOP_ABSENT     1
#define PF_RUMBLE_NOOP_SUPPRESSED 2

/* Session lifecycle. */
PfSession *pf_connect(void);                              /* env-driven; NULL on error */
PfSession *pf_connect_descriptor(const char *path);      /* explicit capabilities.toml */
void       pf_free(PfSession *s);                        /* NULL-safe */

/* Capability queries (side-effect-free). */
PfPresence pf_has_capability(const PfSession *s, const char *name);
int        pf_is_present(const PfSession *s, const char *name);
int        pf_is_granted(const PfSession *s, const char *name);
int        pf_query(const PfSession *s, const char *name);   /* -> PF_GRANTED/DENIED/PROMPT */

/* Acquire + act. */
int  pf_acquire(const PfSession *s, const char *name);       /* -> PF_OK or taxonomy code */
int  pf_rumble_pulse(const PfSession *s, uint32_t ms);       /* -> PF_RUMBLE_* (never fails) */
int  pf_entropy_fill(const PfSession *s, uint8_t *buf, size_t len); /* 0 ok, -1 error */

/* Misc. */
uint32_t    pf_wire_version(void);
const char *pf_strerror(int status);                         /* static; do not free */

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* POCKETFORGE_H */
