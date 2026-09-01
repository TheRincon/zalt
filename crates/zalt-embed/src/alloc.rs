//! Caller-provided memory: the seam a host uses to own what zalt allocates.
//!
//! # Why not a real allocator
//!
//! The obvious reading of "caller-provided allocator" is a per-allocation hook —
//! Rust's `allocator_api`. It is unstable, so a codec built on it only builds on
//! nightly, and a library nobody can ship on stable is not a library. A
//! `#[global_allocator]` is worse: process-wide, so it cannot be scoped to
//! decoders, and it forces `unsafe` into a crate that is `forbid(unsafe_code)`.
//!
//! Buffer donation is the stable answer, and it is what the reference
//! implementation reaches for as well (`ZSTD_initStaticDCtx` and its
//! workspace). The host owns the memory and lends it out; the library never
//! grows anything the host did not hand it beyond what [`Decoder::footprint`]
//! reports. [`zalt_codec::Buffers`] is the currency, and L1 already speaks it —
//! this module is the other half, the part that decides where buffers come from
//! and where they go.
//!
//! [`Decoder::footprint`]: crate::Decoder::footprint
//!
//! # What this buys over just dropping them
//!
//! The workload is many small responses. A decoder returned to the pool above
//! `idle_footprint_max` is shrunk (see [`crate::pool`]), and without a source
//! that memory goes back to the system allocator — so the next large response
//! faults in fresh pages for buffers that existed moments ago. A [`Recycler`]
//! keeps a bounded free list instead, so the peak is *reused* rather than
//! either hoarded per decoder or returned and re-faulted.
//!
//! # Accounting
//!
//! Retained buffers are real resident memory, so a free list that nothing
//! charges makes the budget lie by the size of the free list. A [`Recycler`]
//! built with a [`Budget`] therefore holds its own [`Lease`]: it charges on
//! retain and refunds on hand-out, and when the budget refuses the charge it
//! **drops the buffers instead of retaining them**. A recycler can never push
//! the process over its ceiling; the worst it can do is fail to help.

use core::fmt;
use std::sync::{Arc, Mutex};

use zalt_codec::Buffers;

use crate::budget::{Budget, Lease};

/// Where a decoder gets its working buffers, and where they go afterwards.
///
/// Implement this to hand zalt memory from an arena, a slab, a `mach_vm`
/// region, or anything else the host would rather control than delegate to
/// `malloc`. The default is [`NoSource`], which is exactly today's behaviour:
/// allocate on demand, free on release.
///
/// # Contract
///
/// - [`acquire`](BufferSource::acquire) may return [`Buffers::default`]. Empty
///   is always valid; the decoder grows what it needs.
/// - [`release`](BufferSource::release) takes ownership. Retaining, dropping,
///   or unmapping are all correct responses.
/// - Contents are never meaningful in either direction. **Capacity is what is
///   being lent**, and a decoder clears whatever it adopts, so a source must
///   not rely on buffers coming back with data in them — and must not assume
///   the data it lent is still there.
/// - Both methods take `&self` and may be called from any thread, so a source
///   shared between decoders needs its own interior synchronisation.
pub trait BufferSource: fmt::Debug + Send + Sync {
    /// Supply working buffers for a decoder that is starting or re-arming.
    fn acquire(&self) -> Buffers;

    /// Take back buffers from a decoder that has finished with them.
    fn release(&self, buffers: Buffers);
}

/// The default: the system allocator, with no free list.
///
/// Named rather than implicit so that "no source" is a thing a host can pass
/// deliberately, and so the pool has one code path rather than two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoSource;

impl BufferSource for NoSource {
    fn acquire(&self) -> Buffers {
        Buffers::default()
    }

    /// Drops them. The system allocator is the free list.
    fn release(&self, _buffers: Buffers) {}
}

/// How much a [`Recycler`] may keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecyclerConfig {
    /// Buffer sets kept on the free list. Beyond this, released sets are
    /// dropped.
    pub max_entries: usize,
    /// Total retained capacity in bytes, across all entries.
    ///
    /// This is the number that matters: `max_entries` alone bounds the count,
    /// not the memory, and one entry from a large response can be worth
    /// thousands of small ones.
    pub max_bytes: usize,
}

impl Default for RecyclerConfig {
    fn default() -> Self {
        Self {
            max_entries: 8,
            // Enough for a handful of decoders that each grew to about one
            // maximum block. Large enough to absorb the common peak, small
            // enough that an idle process is not sitting on megabytes.
            max_bytes: 2 * 1024 * 1024,
        }
    }
}

/// A bounded free list of decoder buffers.
///
/// LIFO on purpose: the most recently released set is the most likely to still
/// be resident in cache, and handing back the coldest one first would be the
/// wrong end of a working set that the pool is trying to keep warm.
#[derive(Debug)]
pub struct Recycler {
    config: RecyclerConfig,
    inner: Mutex<Free>,
}

#[derive(Debug)]
struct Free {
    entries: Vec<Buffers>,
    bytes: usize,
    /// Charge against the process budget for what `entries` holds. `None` when
    /// the recycler was built without a budget, in which case the host has
    /// taken on the accounting itself.
    lease: Option<Lease>,
}

impl Recycler {
    /// A recycler that charges nothing.
    ///
    /// Correct only when the host bounds this memory some other way — the
    /// retained bytes are invisible to any [`Budget`]. Prefer
    /// [`Recycler::budgeted`].
    pub fn new(config: RecyclerConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(Free {
                entries: Vec::new(),
                bytes: 0,
                lease: None,
            }),
        }
    }

    /// A recycler whose retained memory is charged to `budget`.
    ///
    /// Returns `None` only if the budget cannot admit a zero-byte reservation,
    /// which means it is already broken.
    pub fn budgeted(config: RecyclerConfig, budget: &Budget) -> Option<Self> {
        let lease = budget.reserve(0)?;
        Some(Self {
            config,
            inner: Mutex::new(Free {
                entries: Vec::new(),
                bytes: 0,
                lease: Some(lease),
            }),
        })
    }

    /// Configuration in force.
    pub fn config(&self) -> RecyclerConfig {
        self.config
    }

    /// Bytes of buffer capacity currently on the free list.
    pub fn retained_bytes(&self) -> usize {
        self.inner.lock().map(|f| f.bytes).unwrap_or(0)
    }

    /// Buffer sets currently on the free list.
    pub fn entry_count(&self) -> usize {
        self.inner.lock().map(|f| f.entries.len()).unwrap_or(0)
    }

    /// Drop everything held, releasing the reservation.
    ///
    /// For a host responding to memory pressure — the free list is pure cache,
    /// so discarding it is always safe and costs only future allocations.
    pub fn purge(&self) {
        if let Ok(mut free) = self.inner.lock() {
            free.entries.clear();
            free.bytes = 0;
            if let Some(lease) = free.lease.as_mut() {
                lease.resize(0);
            }
        }
    }
}

impl Free {
    /// Bring the lease in line with `bytes`, reporting whether it holds.
    ///
    /// Shrinking never fails, so a refusal only ever comes from growth — and
    /// the caller's answer to that is to drop the buffers rather than retain
    /// memory the budget has not authorised.
    fn settle(&mut self) -> bool {
        match self.lease.as_mut() {
            Some(lease) => lease.resize(self.bytes as u64),
            None => true,
        }
    }
}

impl BufferSource for Recycler {
    fn acquire(&self) -> Buffers {
        let Ok(mut free) = self.inner.lock() else {
            return Buffers::default();
        };
        let Some(buffers) = free.entries.pop() else {
            return Buffers::default();
        };
        free.bytes = free.bytes.saturating_sub(buffers.capacity());
        // Shrinking, so this cannot fail.
        free.settle();
        buffers
    }

    fn release(&self, buffers: Buffers) {
        let capacity = buffers.capacity();
        if capacity == 0 {
            return;
        }
        let Ok(mut free) = self.inner.lock() else {
            return;
        };
        if free.entries.len() >= self.config.max_entries
            || free.bytes + capacity > self.config.max_bytes
        {
            return; // Dropped here.
        }

        free.bytes += capacity;
        free.entries.push(buffers);
        if !free.settle() {
            // The budget refused. Retaining anyway would put the process over
            // its ceiling for memory nobody is using, so give it back.
            if let Some(dropped) = free.entries.pop() {
                free.bytes = free.bytes.saturating_sub(dropped.capacity());
            }
            free.settle();
        }
    }
}

/// A [`BufferSource`] handle cheap enough to hand to every decoder.
pub type SharedSource = Arc<dyn BufferSource>;

/// The no-op source, as a [`SharedSource`].
pub fn no_source() -> SharedSource {
    Arc::new(NoSource)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sized(block: usize, window: usize) -> Buffers {
        Buffers {
            block: Vec::with_capacity(block),
            literals: Vec::new(),
            sequences: Vec::new(),

            window: Vec::with_capacity(window),
        }
    }

    #[test]
    fn no_source_hands_out_nothing_and_keeps_nothing() {
        let s = NoSource;
        assert_eq!(s.acquire().capacity(), 0);
        s.release(sized(4096, 4096));
        assert_eq!(s.acquire().capacity(), 0);
    }

    #[test]
    fn a_released_set_comes_back() {
        let r = Recycler::new(RecyclerConfig::default());
        r.release(sized(1024, 2048));
        assert_eq!(r.entry_count(), 1);
        assert!(r.retained_bytes() >= 3072);

        let got = r.acquire();
        assert!(got.capacity() >= 3072, "the capacity must survive the trip");
        assert_eq!(r.entry_count(), 0);
        assert_eq!(r.retained_bytes(), 0);
    }

    #[test]
    fn an_empty_free_list_hands_out_empty_buffers() {
        let r = Recycler::new(RecyclerConfig::default());
        assert_eq!(r.acquire().capacity(), 0);
    }

    /// Empty buffers are not worth an entry: retaining them would burn the
    /// entry budget on sets that donate nothing.
    #[test]
    fn releasing_empty_buffers_is_not_recorded() {
        let r = Recycler::new(RecyclerConfig::default());
        r.release(Buffers::default());
        assert_eq!(r.entry_count(), 0);
    }

    #[test]
    fn the_entry_count_is_bounded() {
        let r = Recycler::new(RecyclerConfig {
            max_entries: 2,
            max_bytes: usize::MAX,
        });
        for _ in 0..5 {
            r.release(sized(64, 64));
        }
        assert_eq!(r.entry_count(), 2);
    }

    /// The bound that actually matters: one entry from a large response is
    /// worth thousands of small ones, so counting entries is not bounding
    /// memory.
    #[test]
    fn the_byte_ceiling_is_bounded_independently_of_the_entry_count() {
        let r = Recycler::new(RecyclerConfig {
            max_entries: 100,
            max_bytes: 8192,
        });
        r.release(sized(4096, 0));
        r.release(sized(4096, 0));
        assert_eq!(r.entry_count(), 2);

        r.release(sized(4096, 0));
        assert_eq!(r.entry_count(), 2, "the third would exceed max_bytes");
        assert!(r.retained_bytes() <= 8192);
    }

    #[test]
    fn most_recently_released_is_handed_out_first() {
        let r = Recycler::new(RecyclerConfig::default());
        r.release(sized(1024, 0));
        r.release(sized(64 * 1024, 0));

        let got = r.acquire();
        assert!(
            got.capacity() >= 64 * 1024,
            "LIFO: the warmest set comes back first, got {}",
            got.capacity()
        );
    }

    #[test]
    fn purge_releases_everything() {
        let budget = Budget::new(1 << 20);
        let r = Recycler::budgeted(RecyclerConfig::default(), &budget).unwrap();
        r.release(sized(4096, 4096));
        assert!(budget.used() > 0);

        r.purge();
        assert_eq!(r.entry_count(), 0);
        assert_eq!(r.retained_bytes(), 0);
        assert_eq!(budget.used(), 0);
    }

    // ------------------------------------------------------------ accounting

    /// A free list nothing charges makes the budget lie by the size of the
    /// free list.
    #[test]
    fn retained_memory_is_charged_to_the_budget() {
        let budget = Budget::new(1 << 20);
        let r = Recycler::budgeted(RecyclerConfig::default(), &budget).unwrap();
        assert_eq!(budget.used(), 0);

        r.release(sized(4096, 4096));
        assert_eq!(
            budget.used(),
            r.retained_bytes() as u64,
            "the budget must see what the free list holds"
        );
    }

    #[test]
    fn handing_a_set_out_refunds_the_budget() {
        let budget = Budget::new(1 << 20);
        let r = Recycler::budgeted(RecyclerConfig::default(), &budget).unwrap();
        r.release(sized(4096, 4096));
        let held = budget.used();
        assert!(held > 0);

        let _got = r.acquire();
        assert_eq!(
            budget.used(),
            0,
            "the decoder is charged for it now, not us"
        );
    }

    /// The property that makes a recycler safe to add to an existing budget: it
    /// can fail to help, but it can never push the process over its ceiling.
    #[test]
    fn a_refused_charge_drops_the_buffers_rather_than_retaining_them() {
        let budget = Budget::new(4096);
        let r = Recycler::budgeted(
            RecyclerConfig {
                max_entries: 100,
                max_bytes: usize::MAX,
            },
            &budget,
        )
        .unwrap();

        r.release(sized(2048, 0));
        assert_eq!(r.entry_count(), 1);

        // This one does not fit the *budget*, though it fits the config.
        r.release(sized(1 << 20, 0));
        assert_eq!(r.entry_count(), 1, "the oversized set must be dropped");
        assert!(budget.used() <= 4096, "budget was {}", budget.used());

        // And the recycler still works afterwards.
        let got = r.acquire();
        assert!(got.capacity() >= 2048);
    }

    #[test]
    fn an_unbudgeted_recycler_still_bounds_itself_by_config() {
        let r = Recycler::new(RecyclerConfig {
            max_entries: 4,
            max_bytes: 4096,
        });
        for _ in 0..20 {
            r.release(sized(2048, 0));
        }
        assert!(r.retained_bytes() <= 4096);
    }

    #[test]
    fn shared_across_threads() {
        let budget = Budget::new(1 << 20);
        let r: Arc<dyn BufferSource> =
            Arc::new(Recycler::budgeted(RecyclerConfig::default(), &budget).unwrap());

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let r = Arc::clone(&r);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let mut b = r.acquire();
                        b.block.reserve(1024);
                        r.release(b);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(budget.used() <= budget.limit());
    }
}
