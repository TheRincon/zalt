//! A pool of reusable decoders.
//!
//! # Why pool at all
//!
//! The workload is many small responses, not a few large ones. A decoder that
//! is allocated, used for four kilobytes, and dropped pays for its block buffer
//! and window every single time — and that allocation cost is a large fraction
//! of the work for a small resource. Resetting keeps the buffers and re-arms the
//! state machine, which is why [`Decoder::reset`] exists and why it is cheap.
//!
//! # Why idle decoders are shrunk
//!
//! A pooled decoder that once served a large response keeps a large buffer, and
//! keeps it *reserved* against the budget while idle. Left alone, a pool
//! converges on every slot holding the high-water mark of anything it ever
//! decoded — memory charged permanently for a peak that has passed.
//!
//! So a decoder returning to the pool above [`PoolConfig::idle_footprint_max`]
//! is shrunk rather than kept. That trades one allocation on its next use for
//! not holding the peak indefinitely, which is the right way round when idle
//! time dominates.

use std::sync::{Arc, Mutex};

use crate::alloc::SharedSource;
use crate::budget::Budget;
use crate::decoder::Decoder;
use crate::policy::Policy;

/// How a pool behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    /// Policy applied to every decoder the pool hands out.
    pub policy: Policy,
    /// Decoders kept ready. Beyond this, returned decoders are dropped.
    pub max_idle: usize,
    /// Footprint above which a returned decoder is shrunk instead of kept.
    pub idle_footprint_max: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            policy: Policy::untrusted(),
            max_idle: 8,
            // Roughly one maximum block. A decoder holding more than that has
            // served something unusual and should not keep the memory.
            idle_footprint_max: 256 * 1024,
        }
    }
}

/// A pool of decoders sharing one memory budget.
#[derive(Debug, Clone)]
pub struct DecoderPool {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: PoolConfig,
    budget: Budget,
    idle: Mutex<Vec<Decoder>>,
    source: Option<SharedSource>,
}

impl DecoderPool {
    /// Create a pool bounded by `budget`.
    pub fn new(config: PoolConfig, budget: Budget) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                budget,
                idle: Mutex::new(Vec::new()),
                source: None,
            }),
        }
    }

    /// Create a pool whose decoders draw their buffers from `source`.
    ///
    /// The two bounds compose rather than overlap. `max_idle` bounds how many
    /// *decoders* stay ready — state machines, warm and immediately usable.
    /// The source bounds how much *memory* survives a decoder being shrunk or
    /// dropped. Without a source, a decoder past `max_idle` takes its buffers
    /// down with it and the next one allocates from scratch; with one, the
    /// pages outlive the decoder that grew them.
    ///
    /// A [`Recycler`](crate::Recycler) built with the same [`Budget`] is the
    /// intended pairing, so that idle buffers stay visible to the accounting
    /// instead of becoming memory nothing reports.
    pub fn with_source(config: PoolConfig, budget: Budget, source: SharedSource) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                budget,
                idle: Mutex::new(Vec::new()),
                source: Some(source),
            }),
        }
    }

    /// The shared budget.
    pub fn budget(&self) -> &Budget {
        &self.inner.budget
    }

    /// The buffer source in force, if any.
    pub fn source(&self) -> Option<&SharedSource> {
        self.inner.source.as_ref()
    }

    /// Configuration in force.
    pub fn config(&self) -> PoolConfig {
        self.inner.config
    }

    /// Number of decoders currently idle.
    pub fn idle_count(&self) -> usize {
        self.inner.idle.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Take a decoder, reusing an idle one when available.
    ///
    /// Returns `None` when the budget cannot admit another decoder — the signal
    /// to apply backpressure rather than start a decode that will fail.
    pub fn get(&self) -> Option<PooledDecoder> {
        let reused = self.inner.idle.lock().ok().and_then(|mut idle| idle.pop());

        let decoder = match reused {
            Some(mut d) => {
                d.reset();
                d
            }
            None => match self.inner.source.clone() {
                Some(source) => {
                    Decoder::with_source(self.inner.config.policy, &self.inner.budget, source)?
                }
                None => Decoder::budgeted(self.inner.config.policy, &self.inner.budget)?,
            },
        };

        Some(PooledDecoder {
            decoder: Some(decoder),
            pool: self.clone(),
        })
    }

    fn give_back(&self, mut decoder: Decoder) {
        // Reset before shrinking, not after. `reset` re-arms the state machine
        // and, when there is a buffer source, refills an empty decoder from it
        // — so shrinking first and resetting second would hand the buffers to
        // the source and take them straight back, and nothing would ever be
        // shrunk. The decoder is re-`reset` on checkout, which is where a
        // shrunk one gets its buffers again.
        decoder.reset();
        if decoder.footprint() > self.inner.config.idle_footprint_max {
            decoder.shrink();
        }

        if let Ok(mut idle) = self.inner.idle.lock() {
            if idle.len() < self.inner.config.max_idle {
                idle.push(decoder);
            }
            // Otherwise it drops here, releasing its lease.
        }
    }
}

/// A decoder borrowed from a [`DecoderPool`], returned on drop.
#[derive(Debug)]
pub struct PooledDecoder {
    // `Option` only so `Drop` can move the decoder back into the pool.
    decoder: Option<Decoder>,
    pool: DecoderPool,
}

impl std::ops::Deref for PooledDecoder {
    type Target = Decoder;
    fn deref(&self) -> &Decoder {
        self.decoder.as_ref().expect("decoder present until drop")
    }
}

impl std::ops::DerefMut for PooledDecoder {
    fn deref_mut(&mut self) -> &mut Decoder {
        self.decoder.as_mut().expect("decoder present until drop")
    }
}

impl Drop for PooledDecoder {
    fn drop(&mut self) {
        if let Some(decoder) = self.decoder.take() {
            self.pool.give_back(decoder);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_ZST: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x0b, 0x59, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20,
        0x77, 0x6f, 0x72, 0x6c, 0x64, 0x68, 0x69, 0x1e, 0xb2,
    ];

    fn pool() -> DecoderPool {
        DecoderPool::new(PoolConfig::default(), Budget::new(64 << 20))
    }

    #[test]
    fn decoders_round_trip_through_the_pool() {
        let p = pool();
        assert_eq!(p.idle_count(), 0);
        {
            let mut d = p.get().unwrap();
            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).unwrap();
            assert_eq!(out, b"hello world");
        }
        assert_eq!(p.idle_count(), 1, "a dropped decoder returns to the pool");
    }

    /// The point of pooling: the second use must not allocate again.
    #[test]
    fn a_reused_decoder_keeps_its_buffers() {
        let p = pool();
        let footprint_after_first = {
            let mut d = p.get().unwrap();
            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).unwrap();
            d.footprint()
        };
        assert!(footprint_after_first > 0);

        let d = p.get().unwrap();
        assert_eq!(
            d.footprint(),
            footprint_after_first,
            "reuse must keep the allocation, not start from zero"
        );
        assert_eq!(p.idle_count(), 0, "it is checked out again");
    }

    #[test]
    fn a_reused_decoder_is_reset_not_stale() {
        let p = pool();
        {
            let mut d = p.get().unwrap();
            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).unwrap();
            assert_eq!(d.decoded_len(), 11);
        }
        let mut d = p.get().unwrap();
        assert_eq!(d.decoded_len(), 0, "state from the last frame must be gone");

        let mut out = Vec::new();
        d.decompress(HELLO_ZST, &mut out).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn the_pool_stops_growing_at_max_idle() {
        let config = PoolConfig {
            max_idle: 2,
            ..Default::default()
        };
        let p = DecoderPool::new(config, Budget::new(64 << 20));

        let handles: Vec<_> = (0..5).map(|_| p.get().unwrap()).collect();
        assert_eq!(p.idle_count(), 0);
        drop(handles);
        assert_eq!(p.idle_count(), 2, "the rest are dropped, not hoarded");
    }

    /// Decoders beyond `max_idle` release their reservations rather than
    /// holding budget while unreachable.
    #[test]
    fn discarded_decoders_release_their_budget() {
        let config = PoolConfig {
            max_idle: 1,
            ..Default::default()
        };
        let budget = Budget::new(64 << 20);
        let p = DecoderPool::new(config, budget.clone());

        let mut handles: Vec<_> = (0..4).map(|_| p.get().unwrap()).collect();
        for h in handles.iter_mut() {
            let mut out = Vec::new();
            h.decompress(HELLO_ZST, &mut out).unwrap();
        }
        let peak = budget.used();
        assert!(peak > 0);

        drop(handles);
        assert_eq!(p.idle_count(), 1);
        assert!(
            budget.used() < peak,
            "discarded decoders must return their reservations"
        );
    }

    /// An idle decoder must not hold the high-water mark of everything it ever
    /// decoded.
    #[test]
    fn a_large_decoder_is_shrunk_before_going_idle() {
        let config = PoolConfig {
            idle_footprint_max: 4,
            ..Default::default()
        };
        let budget = Budget::new(64 << 20);
        let p = DecoderPool::new(config, budget.clone());

        {
            let mut d = p.get().unwrap();
            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).unwrap();
            assert!(d.footprint() > 4);
        }

        let d = p.get().unwrap();
        assert_eq!(d.footprint(), 0, "an oversized decoder must be shrunk");
        assert_eq!(d.reserved(), 0, "and must not keep its reservation");
    }

    #[test]
    fn an_exhausted_budget_refuses_a_new_decoder() {
        let budget = Budget::new(0);
        let p = DecoderPool::new(PoolConfig::default(), budget.clone());
        // A zero-byte reservation still fits a zero budget; the refusal comes
        // when the decoder tries to grow.
        let mut d = p.get().unwrap();
        let mut out = Vec::new();
        assert!(d.decompress(HELLO_ZST, &mut out).is_err());
    }

    // ------------------------------------------------------- buffer sources

    mod source {
        use super::*;
        use crate::alloc::{Recycler, RecyclerConfig};
        use std::sync::Arc;

        fn pooled(idle_footprint_max: usize) -> (DecoderPool, Arc<Recycler>, Budget) {
            let budget = Budget::new(64 << 20);
            let r = Arc::new(Recycler::budgeted(RecyclerConfig::default(), &budget).unwrap());
            let config = PoolConfig {
                idle_footprint_max,
                ..Default::default()
            };
            let pool = DecoderPool::with_source(config, budget.clone(), r.clone());
            (pool, r, budget)
        }

        /// Without a source, shrinking a returned decoder hands the memory back
        /// to the system allocator and the next large response faults it in
        /// again. With one, the peak survives the decoder that grew it.
        #[test]
        fn a_shrunk_decoder_leaves_its_buffers_with_the_source() {
            let (p, r, _budget) = pooled(4);
            {
                let mut d = p.get().unwrap();
                let mut out = Vec::new();
                d.decompress(HELLO_ZST, &mut out).unwrap();
                assert!(d.footprint() > 4);
            }
            assert!(
                r.retained_bytes() > 0,
                "shrinking must route through the source"
            );
        }

        #[test]
        fn the_next_checkout_gets_the_recycled_buffers() {
            let (p, r, _budget) = pooled(4);
            let grown = {
                let mut d = p.get().unwrap();
                let mut out = Vec::new();
                d.decompress(HELLO_ZST, &mut out).unwrap();
                d.footprint()
            };

            let d = p.get().unwrap();
            assert_eq!(
                d.footprint(),
                grown,
                "a checked-out decoder must come back warm"
            );
            assert_eq!(r.retained_bytes(), 0, "the free list handed them over");
        }

        /// Decoders discarded past `max_idle` are the case a source most
        /// changes: without one their buffers die with them.
        #[test]
        fn decoders_discarded_past_max_idle_still_return_their_buffers() {
            let budget = Budget::new(64 << 20);
            let r = Arc::new(Recycler::budgeted(RecyclerConfig::default(), &budget).unwrap());
            let config = PoolConfig {
                max_idle: 1,
                ..Default::default()
            };
            let p = DecoderPool::with_source(config, budget.clone(), r.clone());

            let mut handles: Vec<_> = (0..4).map(|_| p.get().unwrap()).collect();
            for h in handles.iter_mut() {
                let mut out = Vec::new();
                h.decompress(HELLO_ZST, &mut out).unwrap();
            }
            drop(handles);

            assert_eq!(p.idle_count(), 1);
            assert!(
                r.retained_bytes() > 0,
                "the three discarded decoders must not take their memory with them"
            );
        }

        #[test]
        fn output_is_still_correct_through_recycled_buffers() {
            let (p, _r, _budget) = pooled(4);
            for _ in 0..50 {
                let mut d = p.get().unwrap();
                let mut out = Vec::new();
                d.decompress(HELLO_ZST, &mut out).unwrap();
                assert_eq!(out, b"hello world");
            }
        }

        #[test]
        fn the_budget_does_not_drift_under_concurrent_churn() {
            let (p, r, budget) = pooled(4);

            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let p = p.clone();
                    std::thread::spawn(move || {
                        for _ in 0..100 {
                            let mut d = p.get().expect("budget is ample");
                            let mut out = Vec::new();
                            d.decompress(HELLO_ZST, &mut out).unwrap();
                            assert_eq!(out, b"hello world");
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }

            assert!(budget.used() <= budget.limit());
            let held: u64 = r.retained_bytes() as u64;
            assert!(
                budget.used() >= held,
                "the free list is charged: used {} < retained {}",
                budget.used(),
                held
            );
        }

        /// The pool must work unchanged when the source has nothing to give —
        /// an exhausted free list is the steady state under load, not an error.
        #[test]
        fn an_empty_source_is_not_an_error() {
            let budget = Budget::new(64 << 20);
            let r = Arc::new(Recycler::new(RecyclerConfig {
                max_entries: 0,
                max_bytes: 0,
            }));
            let p = DecoderPool::with_source(PoolConfig::default(), budget, r.clone());

            let mut d = p.get().unwrap();
            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).unwrap();
            assert_eq!(out, b"hello world");
            assert_eq!(r.retained_bytes(), 0);
        }
    }

    #[test]
    fn shared_across_threads() {
        let p = pool();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let p = p.clone();
                std::thread::spawn(move || {
                    let mut d = p.get().expect("budget is ample");
                    let mut out = Vec::new();
                    d.decompress(HELLO_ZST, &mut out).unwrap();
                    out
                })
            })
            .collect();

        for h in handles {
            assert_eq!(h.join().unwrap(), b"hello world");
        }
        assert!(p.idle_count() <= p.config().max_idle);
        assert_eq!(p.budget().used(), p.budget().used(), "no accounting drift");
    }
}
