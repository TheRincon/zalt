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
//!
//! # Why the idle set is sharded, and the decoders boxed
//!
//! Both measured with `zalt-bench concurrent` on the small `.zdoc` documents
//! that are §8's actual workload, against a control of one decoder per thread
//! that shares nothing (`doc/ZALT-MAC.md` §10):
//!
//! | idle set | pooled, 8 threads | cost against the control |
//! |---|---:|---:|
//! | one `Mutex<Vec<Decoder>>` | ~946 MB/s | 0.84x |
//! | sharded per thread | ~1 038 MB/s | 0.92x |
//! | sharded, decoders boxed | ~1 123 MB/s | **1.00x** |
//!
//! The two costs were different in kind, and the measurement only separated
//! them because the pool was ~7% behind the control **at one thread**, where
//! there is nothing to contend with.
//!
//! - **Contention** grew with thread count. The lock is taken twice per
//!   resource, and with few-kilobyte resources those acquisitions are
//!   microseconds apart. Each thread now reaches for a shard of its own first;
//!   a miss falls through to the others with `try_lock`, so it never blocks.
//!   `max_idle` stays one exact global cap held in an atomic — a per-shard cap
//!   would be a memory ceiling that moves with how many threads touched the
//!   pool.
//! - **Copying** was flat. A `Decoder` carries its entropy tables inline and is
//!   about 14 KB, and a checkout moved it by value four to six times; profiled
//!   side by side, the only difference from the control was 14% more
//!   `_platform_memmove`. Boxing makes each move a pointer, for one allocation
//!   per decoder created rather than per resource served.
//!
//! An earlier attempt at sharding measured nothing and was reverted, on a
//! machine with its swap file 94% full; on a quiet one it is reproducible to
//! about 1%. The table above is from the quiet one.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    /// Idle decoders, split so threads rarely touch the same lock.
    shards: Vec<Mutex<Vec<Box<Decoder>>>>,
    /// Total decoders across every shard. Authoritative for admission, so
    /// `max_idle` means the same thing however the threads land on shards.
    idle_total: AtomicUsize,
    source: Option<SharedSource>,
}

/// One shard per core, bounded.
fn shard_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
}

thread_local! {
    /// This thread's preferred shard, fixed on first use.
    static PREFERRED_SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Hands out a distinct starting shard to each thread that asks.
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

fn preferred_shard(shards: usize) -> usize {
    PREFERRED_SHARD.with(|cell| {
        let mut index = cell.get();
        if index == usize::MAX {
            index = NEXT_SHARD.fetch_add(1, Ordering::Relaxed);
            cell.set(index);
        }
        index % shards.max(1)
    })
}

impl DecoderPool {
    /// Create a pool bounded by `budget`.
    pub fn new(config: PoolConfig, budget: Budget) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                budget,
                shards: (0..shard_count()).map(|_| Mutex::new(Vec::new())).collect(),
                idle_total: AtomicUsize::new(0),
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
                shards: (0..shard_count()).map(|_| Mutex::new(Vec::new())).collect(),
                idle_total: AtomicUsize::new(0),
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
        self.inner
            .shards
            .iter()
            .map(|s| s.lock().map(|g| g.len()).unwrap_or(0))
            .sum()
    }

    /// Take a decoder, reusing an idle one when available.
    ///
    /// Returns `None` when the budget cannot admit another decoder — the signal
    /// to apply backpressure rather than start a decode that will fail.
    pub fn get(&self) -> Option<PooledDecoder> {
        let reused = self.take_idle();

        let decoder = match reused {
            Some(mut d) => {
                d.reset();
                d
            }
            None => Box::new(match self.inner.source.clone() {
                Some(source) => {
                    Decoder::with_source(self.inner.config.policy, &self.inner.budget, source)?
                }
                None => Decoder::budgeted(self.inner.config.policy, &self.inner.budget)?,
            }),
        };

        Some(PooledDecoder {
            decoder: Some(decoder),
            pool: self.clone(),
        })
    }

    /// Take an idle decoder, preferring this thread's own shard. Other shards
    /// are tried with `try_lock`, so a miss never blocks.
    fn take_idle(&self) -> Option<Box<Decoder>> {
        let shards = &self.inner.shards;
        let start = preferred_shard(shards.len());
        if let Ok(mut own) = shards[start].lock()
            && let Some(decoder) = own.pop()
        {
            self.inner.idle_total.fetch_sub(1, Ordering::Relaxed);
            return Some(decoder);
        }
        for offset in 1..shards.len() {
            let index = (start + offset) % shards.len();
            if let Ok(mut other) = shards[index].try_lock()
                && let Some(decoder) = other.pop()
            {
                self.inner.idle_total.fetch_sub(1, Ordering::Relaxed);
                return Some(decoder);
            }
        }
        None
    }

    /// Claim a slot under the global `max_idle` cap.
    fn claim_idle_slot(&self) -> bool {
        let max = self.inner.config.max_idle;
        let mut current = self.inner.idle_total.load(Ordering::Relaxed);
        loop {
            if current >= max {
                return false;
            }
            match self.inner.idle_total.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn give_back(&self, mut decoder: Box<Decoder>) {
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

        if !self.claim_idle_slot() {
            // Drops here, releasing its lease.
            return;
        }
        let shards = &self.inner.shards;
        let index = preferred_shard(shards.len());
        match shards[index].lock() {
            Ok(mut shard) => shard.push(decoder),
            // Poisoned: the decoder drops, so the claimed slot goes back.
            Err(_) => {
                self.inner.idle_total.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// A decoder borrowed from a [`DecoderPool`], returned on drop.
#[derive(Debug)]
pub struct PooledDecoder {
    // `Option` only so `Drop` can move the decoder back into the pool.
    //
    // Boxed, and the reason is measured rather than tidy. A `Decoder` carries
    // its entropy tables inline and is about 14 KB, and a checkout moves it by
    // value several times over — out of the shard's `Vec`, into this struct,
    // out again on drop, into `give_back`, back into the `Vec`. On §8's
    // workload of few-kilobyte resources that was *more bytes copied than
    // decoded*: `_platform_memmove` ran 14% higher pooled than with a decoder
    // per thread, and the pool cost 7–8% at one thread where there is nothing
    // to contend with. A box makes every one of those moves a pointer, for one
    // allocation per decoder *created* rather than per resource served.
    decoder: Option<Box<Decoder>>,
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

    /// `max_idle` is one global cap, however the threads land on shards.
    ///
    /// The invariant sharding could most easily break: with a per-shard cap, or
    /// a counter that drifts from what the shards hold, the pool's memory
    /// ceiling would depend on how many threads happened to use it. Many threads
    /// each hold several decoders at once and return them together, so returns
    /// arrive on every shard at the same moment.
    #[test]
    fn max_idle_is_exact_across_shards_under_concurrent_returns() {
        let config = PoolConfig {
            max_idle: 3,
            ..Default::default()
        };
        let p = DecoderPool::new(config, Budget::new(1 << 30));

        for _ in 0..20 {
            let barrier = Arc::new(std::sync::Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let p = p.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        // Each decodes a frame first, so it holds real buffers
                        // and a real reservation — an untouched decoder holds
                        // neither, and the budget check below would pass on a
                        // leak.
                        let held: Vec<_> = (0..4)
                            .map(|_| {
                                let mut d = p.get().expect("ample");
                                let mut out = Vec::new();
                                d.decompress(HELLO_ZST, &mut out).expect("decodes");
                                d
                            })
                            .collect();
                        barrier.wait();
                        drop(held);
                    })
                })
                .collect();
            for h in handles {
                h.join().expect("no panic");
            }
            assert_eq!(
                p.idle_count(),
                3,
                "32 decoders returned at once must leave exactly max_idle parked"
            );
        }

        // And the 29 that were not parked released their reservations. The
        // reference is measured in a *separate* pool, so it cannot be derived
        // from the number it checks: one decoder, having decoded the same
        // frame, held and charged.
        let one = {
            let reference = DecoderPool::new(config, Budget::new(1 << 30));
            let mut d = reference.get().expect("ample");
            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).expect("decodes");
            reference.budget().used()
        };
        assert!(one > 0, "a decoder that has decoded holds a reservation");
        assert_eq!(
            p.budget().used(),
            3 * one,
            "exactly the three parked decoders are charged; a leak would be 32"
        );
        drop(p.get());
        assert_eq!(
            p.budget().used(),
            3 * one,
            "a checkout and return must leave the reservation where it was"
        );
    }
}
