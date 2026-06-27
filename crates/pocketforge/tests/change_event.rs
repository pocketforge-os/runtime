//! The Permissions-API `query()` **change event**: a subscriber observes a permission state
//! transition when consent is granted/revoked (the E3 seam). Demonstrated on the in-process
//! backend (the cooperative v0 control plane = the sim's injection-as-API control surface).

mod common;

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use pocketforge::backends::InProcessBackend;
use pocketforge::{CapError, Imu, Location, PermissionState, Pf};

#[test]
fn location_consent_grant_fires_change_event_and_flips_query() {
    // Uses the synthetic GNSS descriptor (no shipping device advertises GNSS yet) to exercise
    // the consent state machine + change event for a default-deny privacy cap.
    let backend = InProcessBackend::shared(Arc::new(common::gnss_descriptor()));
    let pf = Pf::over_in_process(backend.clone());

    // Subscribe BEFORE the change.
    let rx = backend.subscribe("location");

    // Default-deny: query is Prompt, acquire is consent-denied.
    assert_eq!(pf.query::<Location>(), PermissionState::Prompt);
    assert_eq!(pf.acquire::<Location>().err(), Some(CapError::ConsentDenied));

    // The consent layer (E3) grants it → the change event fires AND query() flips to Granted.
    backend.set_consent("location", PermissionState::Granted);
    let evt = rx.recv_timeout(Duration::from_secs(1)).expect("change event delivered");
    assert_eq!(evt, PermissionState::Granted);
    assert_eq!(pf.query::<Location>(), PermissionState::Granted);
    assert!(pf.acquire::<Location>().is_ok(), "granted consent ⇒ acquire succeeds");

    // Revoke → another event, query back to Denied.
    backend.set_consent("location", PermissionState::Denied);
    let evt = rx.recv_timeout(Duration::from_secs(1)).expect("revoke event delivered");
    assert_eq!(evt, PermissionState::Denied);
    assert_eq!(pf.query::<Location>(), PermissionState::Denied);
}

#[test]
fn no_event_without_a_change() {
    let backend = InProcessBackend::shared(Arc::new(common::descriptor("a523")));
    let rx = backend.subscribe("imu");
    // Nothing changed → no spurious event.
    assert_eq!(rx.recv_timeout(Duration::from_millis(100)), Err(RecvTimeoutError::Timeout));
    // (imu starts granted on a523; sanity-check that read path is unaffected.)
    let pf = Pf::over_in_process(backend);
    assert!(pf.acquire::<Imu>().is_ok());
}
