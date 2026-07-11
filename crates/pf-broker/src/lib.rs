//! # `pf-broker` — the enforcing capability-broker daemon core (`tsp-e1b.3`)
//!
//! The default-deny daemon that owns the real device fds and vends brokered handles to apps over
//! the `.2` PFW1 wire. It is the privileged trust-path piece of E2: small, audited, default-deny.
//! Three things make it a BROKER rather than the cooperative `pf-broker-ref` reference loopback:
//!
//!   1. **launch-time `app.toml` validation** ([`AppManifest::validate`]) — the `use = [...]`
//!      CapDL-style authority graph is checked against the device descriptor; over-broad /
//!      dangling / unknown / undescriptored-required routes are REJECTED before the app runs;
//!   2. **runtime enforcement** ([`EnforcingBackend`]) — the validated manifest is the CEILING:
//!      an op outside it is `PolicyBlocked`/`Denied`, default-deny is preserved, dangerous caps
//!      are quota-capped, and `entropy` is the deliberate ungated exception;
//!   3. **peer-credential checks** ([`serve::peer_cred`], `SO_PEERCRED`) at accept time.
//!
//! ## Substrate reality (R-A) — what is and isn't enforced in v0
//!
//! [`EnforcingBackend`] is a [`Backend`](pocketforge::Backend), so the `.2` wire server serves it
//! unchanged and E6 swaps from the in-process backend to this socket with NO app-source change
//! (the load-bearing "survives the runtime fork" demo, now enforcing). What v0 enforces: the
//! authority graph, default-deny, quotas, and peer-uid — **cooperatively over the socket**. What
//! it does NOT yet do: confine a process that ignores the socket and reaches `/dev/*` directly —
//! that needs namespaces/seccomp (unbuilt owned kernel M2.B-E + paused M1.D supervisor). FULL
//! fd-routing into a real app namespace is the substrate-gated follow-on leg, named not papered
//! over. See `docs/BROKER-DESIGN.md`.

pub mod enforce;
pub mod manifest;
pub mod serve;
pub mod tier;

pub use enforce::EnforcingBackend;
pub use manifest::{
    AppManifest, AppSection, BlessedRegistration, LaunchTrust, UseEntry, ValidatedManifest, Violation,
};
pub use serve::{peer_cred, serve_enforcing, serve_enforcing_until, PeerCred};
pub use tier::{tier_of, Tier};
