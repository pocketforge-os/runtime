//! Consolidated integration target for session-authority orientation policy, Linux inputs, and
//! daemon lifecycle isolation. Keeping these suites in one crate avoids relinking the same runtime
//! dependency graph once per source file in test, cross-build, and clippy profiles.

#[path = "orientation/daemon_initial_state.rs"]
mod daemon_initial_state;
#[path = "orientation/daemon_slow_client.rs"]
mod daemon_slow_client;
#[path = "orientation/policy.rs"]
mod policy;
#[path = "orientation/source.rs"]
mod source;
