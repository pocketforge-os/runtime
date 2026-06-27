//! The **entropy manager** — the deliberate **ungated** capability (normal tier: auto-grant, no
//! quota, no consent). Reads the OS CSPRNG (`/dev/urandom`) directly. The broker never
//! rate-limits entropy; the boot-seed-quality probe is a separate E1/R-G follow-up, not this path.

use std::io::Read;
use std::sync::Arc;

use crate::backend::Backend;
use crate::error::CapError;

/// One device-agnostic entropy source.
pub struct EntropyManager {
    backend: Arc<dyn Backend>,
}

impl EntropyManager {
    /// Build the manager from a session's backend.
    pub fn new(backend: Arc<dyn Backend>) -> EntropyManager {
        EntropyManager { backend }
    }

    /// Fill `buf` with cryptographically-strong random bytes. Ungated: `acquire` always grants on
    /// both backends, then we read the OS CSPRNG.
    pub fn fill(&self, buf: &mut [u8]) -> Result<(), CapError> {
        // Go through the backend so the (ungated) grant decision is observed identically over the
        // backend swap; entropy is never default-deny so this is always Ok.
        self.backend.acquire("entropy")?;
        let mut f = std::fs::File::open("/dev/urandom").map_err(|_| CapError::HardwareAbsent)?;
        f.read_exact(buf).map_err(|_| CapError::HardwareAbsent)
    }
}
