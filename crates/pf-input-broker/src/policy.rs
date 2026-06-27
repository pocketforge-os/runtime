//! Broker-side input **policy** — the rate limiter the pump applies before re-emitting. A token
//! bucket bounds the event rate a (cooperative or hostile) source can push through the grab, so a
//! flooding device cannot wedge the consumer. Pure + explicit-time so it is deterministically
//! testable; the pump feeds it the monotonic elapsed seconds. The default cap is generous (it
//! never trips on normal gameplay); it exists to bound abuse, not to shape input.

/// A simple token bucket: `capacity` tokens, refilled at `refill_per_sec`, one token per event.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_secs: f64,
}

impl TokenBucket {
    /// A bucket holding `capacity` events, refilling at `refill_per_sec`. Starts full.
    pub fn new(capacity: f64, refill_per_sec: f64) -> TokenBucket {
        TokenBucket { capacity, refill_per_sec, tokens: capacity, last_secs: 0.0 }
    }

    /// The broker default: 4000 events buffered, 4000/sec sustained — far above a 60 Hz gamepad's
    /// handful of events/frame, so normal input is never dropped; a runaway flood is.
    pub fn default_broker() -> TokenBucket {
        TokenBucket::new(4000.0, 4000.0)
    }

    /// Account one event at monotonic time `now_secs`. Returns `true` if a token was available
    /// (emit), `false` if the bucket is empty (drop). Monotonic input assumed (`now ≥ last`).
    pub fn allow(&mut self, now_secs: f64) -> bool {
        let dt = (now_secs - self.last_secs).max(0.0);
        self.last_secs = now_secs;
        self.tokens = (self.tokens + dt * self.refill_per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_bucket_passes_then_empties_then_refills() {
        let mut b = TokenBucket::new(3.0, 1.0);
        // Three events at t=0 fit; the fourth is dropped.
        assert!(b.allow(0.0));
        assert!(b.allow(0.0));
        assert!(b.allow(0.0));
        assert!(!b.allow(0.0), "bucket empty → drop");
        // One second later one token has refilled.
        assert!(b.allow(1.0));
        assert!(!b.allow(1.0));
    }

    #[test]
    fn default_never_drops_normal_gameplay() {
        // 60 Hz × ~10 events/frame for a second = 600 events; the default (4000) absorbs it.
        let mut b = TokenBucket::default_broker();
        let mut dropped = 0;
        for i in 0..600 {
            if !b.allow(i as f64 / 600.0) {
                dropped += 1;
            }
        }
        assert_eq!(dropped, 0);
    }
}
