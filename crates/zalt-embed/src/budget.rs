//! Global memory accounting across live decoders.
//!
//! A per-frame window cap bounds one decoder. It does not bound a host that
//! creates many. A page referencing a hundred compressed subresources, decoded
//! concurrently, authorises a hundred times the per-frame cap — so the per-frame
//! check is necessary and not sufficient (`doc/ZALT-MAC.md` §6).
//!
//! [`Budget`] is the ceiling on the sum. Decoders take a [`Lease`] before
//! reserving memory and the lease releases on drop, so a decoder that panics or
//! is dropped mid-frame cannot leak reservation.

use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A shared ceiling on total live decoder memory.
#[derive(Debug, Clone)]
pub struct Budget {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    limit: u64,
    used: AtomicU64,
}

impl Budget {
    /// Create a budget with the given ceiling in bytes.
    pub fn new(limit: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                limit,
                used: AtomicU64::new(0),
            }),
        }
    }

    /// Try to reserve `bytes`. Returns `None` when the budget is exhausted.
    ///
    /// Refusal is the signal to apply backpressure — queue the response, or
    /// fail the resource — not to allocate anyway.
    pub fn reserve(&self, bytes: u64) -> Option<Lease> {
        let mut current = self.inner.used.load(Ordering::Relaxed);
        loop {
            let next = current.checked_add(bytes)?;
            if next > self.inner.limit {
                return None;
            }
            match self.inner.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(Lease {
                        budget: self.clone(),
                        bytes,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Bytes currently reserved.
    pub fn used(&self) -> u64 {
        self.inner.used.load(Ordering::Acquire)
    }

    /// The ceiling.
    pub fn limit(&self) -> u64 {
        self.inner.limit
    }

    /// Bytes still available.
    pub fn available(&self) -> u64 {
        self.inner.limit.saturating_sub(self.used())
    }
}

/// A reservation against a [`Budget`], released on drop.
#[derive(Debug)]
pub struct Lease {
    budget: Budget,
    bytes: u64,
}

impl Lease {
    /// Size of this reservation.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Adjust this reservation to `bytes`, returning whether it succeeded.
    ///
    /// Growing can fail; shrinking never does. A refusal leaves the lease at
    /// its previous size, so a caller that cannot grow still holds what it had.
    ///
    /// # Why leases resize at all
    ///
    /// A decoder's footprint is not known when it starts. The window is
    /// allocated lazily — a frame declaring 8 MiB but producing 4 KB holds
    /// 4 KB — so reserving the declared maximum up front would charge every
    /// response for memory it will never touch, and defeat the lazy allocation
    /// that makes many concurrent small responses affordable in the first place.
    ///
    /// The alternative, reserving nothing and hoping, is not accounting at all.
    /// So the reservation tracks actual usage and the caller re-checks as it
    /// grows.
    pub fn resize(&mut self, bytes: u64) -> bool {
        if bytes <= self.bytes {
            let released = self.bytes - bytes;
            self.budget.inner.used.fetch_sub(released, Ordering::AcqRel);
            self.bytes = bytes;
            return true;
        }

        let additional = bytes - self.bytes;
        let mut current = self.budget.inner.used.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_add(additional) else {
                return false;
            };
            if next > self.budget.inner.limit {
                return false;
            }
            match self.budget.inner.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.bytes = bytes;
                    return true;
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.budget
            .inner
            .used
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_and_releases() {
        let b = Budget::new(1000);
        let l = b.reserve(400).unwrap();
        assert_eq!(b.used(), 400);
        assert_eq!(b.available(), 600);
        drop(l);
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn refuses_past_the_ceiling() {
        let b = Budget::new(1000);
        let _a = b.reserve(600).unwrap();
        assert!(b.reserve(600).is_none());
        assert_eq!(b.used(), 600, "a refused reservation must not be charged");
    }

    /// The scenario the budget exists for: one page, many subresources, each
    /// individually within the per-frame window cap.
    #[test]
    fn many_small_decoders_hit_the_global_ceiling() {
        let per_frame = 8 * 1024 * 1024;
        let b = Budget::new(64 * 1024 * 1024);
        let leases: Vec<_> = (0..8).map(|_| b.reserve(per_frame).unwrap()).collect();
        assert_eq!(leases.len(), 8);
        assert!(
            b.reserve(per_frame).is_none(),
            "the 9th concurrent decoder must be refused"
        );
        drop(leases);
        assert!(b.reserve(per_frame).is_some());
    }

    #[test]
    fn overflow_cannot_wrap_the_counter() {
        let b = Budget::new(u64::MAX);
        let _l = b.reserve(u64::MAX - 1).unwrap();
        assert!(b.reserve(u64::MAX).is_none());
    }

    #[test]
    fn lease_grows_within_the_budget() {
        let b = Budget::new(1000);
        let mut l = b.reserve(100).unwrap();
        assert!(l.resize(400));
        assert_eq!(l.bytes(), 400);
        assert_eq!(b.used(), 400);
    }

    #[test]
    fn lease_shrinks_and_returns_the_difference() {
        let b = Budget::new(1000);
        let mut l = b.reserve(800).unwrap();
        assert!(l.resize(200));
        assert_eq!(b.used(), 200);
        assert_eq!(b.available(), 800);
    }

    /// A refused growth must leave the lease exactly as it was — a caller that
    /// cannot expand still holds what it already had, and must not find its
    /// reservation silently reduced or double-counted.
    #[test]
    fn refused_growth_leaves_the_lease_intact() {
        let b = Budget::new(1000);
        let _other = b.reserve(700).unwrap();
        let mut l = b.reserve(100).unwrap();

        assert!(!l.resize(500), "500 + 700 exceeds 1000");
        assert_eq!(l.bytes(), 100, "lease size unchanged");
        assert_eq!(b.used(), 800, "budget unchanged");

        // And the lease is still usable afterwards.
        assert!(l.resize(200));
        assert_eq!(b.used(), 900);
    }

    #[test]
    fn resized_lease_releases_everything_on_drop() {
        let b = Budget::new(1000);
        let mut l = b.reserve(100).unwrap();
        l.resize(600);
        drop(l);
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn shared_across_threads() {
        let b = Budget::new(1000);
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let b = b.clone();
                std::thread::spawn(move || b.reserve(200))
            })
            .collect();
        let granted: Vec<_> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        // Exactly five 200-byte leases fit in 1000; the rest must be refused.
        assert_eq!(granted.len(), 5);
        assert_eq!(b.used(), 1000);
    }
}
