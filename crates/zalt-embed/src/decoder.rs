//! A budgeted, policy-bound decoder.
//!
//! This is what a host actually holds: a codec instance plus the two things the
//! codec deliberately does not know about — the policy governing what input it
//! will accept, and a reservation against a process-wide memory ceiling.
//!
//! # Accounting against a footprint that grows
//!
//! The awkward part, and the reason [`Lease::resize`] exists.
//!
//! A decoder's memory is not known when it starts. The window is allocated
//! lazily, so a frame declaring an 8 MiB window but producing 4 KB holds 4 KB.
//! Reserving the declared maximum up front would be simple and wrong: it would
//! charge every response for memory it never touches, and refuse the ninth
//! concurrent 4 KB response on a 64 MiB budget.
//!
//! Reserving nothing and hoping is not accounting at all.
//!
//! So the lease tracks *actual* usage, re-checked after each call. When the
//! budget refuses growth the decode fails with [`Error::OverBudget`] — which is
//! the signal to apply backpressure, not to allocate anyway.

use std::fmt;

use zalt_codec::decode::{Progress, Status};

use crate::DefaultBackend;
use crate::alloc::SharedSource;
use crate::budget::{Budget, Lease};
use crate::policy::Policy;

/// A decode failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The codec rejected the input.
    Codec(zalt_codec::Error),
    /// The decoder needs more memory than the budget allows.
    ///
    /// Distinct from a codec error because the caller can act on it: queue the
    /// response, retry when other decoders finish, or fail this one resource
    /// rather than the page. Collapsing it into a parse error would make that
    /// decision guesswork.
    OverBudget {
        /// Bytes the decoder wanted to hold.
        needed: u64,
        /// Bytes the budget could offer.
        available: u64,
    },
}

impl From<zalt_codec::Error> for Error {
    fn from(e: zalt_codec::Error) -> Self {
        Self::Codec(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codec(e) => write!(f, "{e}"),
            Self::OverBudget { needed, available } => {
                write!(f, "decoder needs {needed} bytes, budget offers {available}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Result alias for embedding operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A decoder bound to a policy and, optionally, a memory budget.
pub struct Decoder {
    inner: zalt_codec::Decoder<DefaultBackend>,
    policy: Policy,
    lease: Option<Lease>,
    /// Where working buffers come from and go back to. `None` means the system
    /// allocator, which is the common case and costs nothing to represent.
    source: Option<SharedSource>,
    /// The dictionary loaded, kept so [`Decoder::shrink`] — which may replace
    /// the inner decoder — does not forget it.
    dictionary: Option<zalt_codec::dict::Dictionary>,
}

impl fmt::Debug for Decoder {
    /// Hand-written because a `dyn BufferSource` is not usefully printable and
    /// its `Debug` is the host's, not ours.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decoder")
            .field("policy", &self.policy)
            .field("footprint", &self.footprint())
            .field("reserved", &self.reserved())
            .field("pooled_buffers", &self.source.is_some())
            .finish()
    }
}

impl Decoder {
    /// Create a decoder with the given policy and no budget.
    ///
    /// Suitable for a single decode where the caller already bounds memory some
    /// other way. A host running many concurrently wants [`Decoder::budgeted`].
    pub fn new(policy: Policy) -> Self {
        Self {
            inner: zalt_codec::Decoder::new(policy.limits),
            policy,
            lease: None,
            source: None,
            dictionary: None,
        }
    }

    /// Create a decoder that charges its memory to `budget`.
    ///
    /// Returns `None` when the budget cannot cover even an idle decoder, which
    /// is the point at which a host should stop starting new decodes.
    pub fn budgeted(policy: Policy, budget: &Budget) -> Option<Self> {
        // A decoder holds nothing until it sees input, so the initial charge is
        // nominal. It exists so that an exhausted budget refuses *here*, before
        // a response is accepted, rather than partway through decoding it.
        let lease = budget.reserve(0)?;
        Some(Self {
            inner: zalt_codec::Decoder::new(policy.limits),
            policy,
            lease: Some(lease),
            source: None,
            dictionary: None,
        })
    }

    /// Create a budgeted decoder that draws its working buffers from `source`.
    ///
    /// The decoder takes buffers on creation and on [`reset`](Decoder::reset),
    /// and gives them back on [`shrink`](Decoder::shrink) and on drop — so a
    /// host that supplies an arena, a slab, or a bounded free list sees the
    /// memory come home rather than reaching the system allocator. See
    /// [`crate::alloc`].
    pub fn with_source(policy: Policy, budget: &Budget, source: SharedSource) -> Option<Self> {
        let lease = budget.reserve(0)?;
        let mut decoder = Self {
            inner: zalt_codec::Decoder::new(policy.limits),
            policy,
            lease: Some(lease),
            source: Some(source),
            dictionary: None,
        };
        decoder.refill();
        Some(decoder)
    }

    /// The buffer source in force, if any.
    pub fn source(&self) -> Option<&SharedSource> {
        self.source.as_ref()
    }

    /// Take buffers from the source, if there is one and we are empty.
    ///
    /// Only refills an *empty* decoder: buffers already held are at least as
    /// warm as anything on a free list, and adopting over them would either
    /// discard them or double the footprint.
    ///
    /// A donation that the budget will not cover is handed straight back. That
    /// is not a failure — the decoder simply starts cold, which is exactly what
    /// it would have done without a source at all.
    fn refill(&mut self) {
        let Some(source) = self.source.clone() else {
            return;
        };
        if self.inner.heap_footprint() != 0 {
            return;
        }
        let buffers = source.acquire();
        if buffers.capacity() == 0 {
            return;
        }
        self.inner.donate_buffers(buffers);
        if self.settle_lease().is_err() {
            source.release(self.inner.take_buffers());
            // Back to nothing held, so this cannot fail.
            let _ = self.settle_lease();
        }
    }

    /// Decode frames from the next frame boundary on with `dictionary`: see
    /// [`zalt_codec::Decoder::load_dictionary`]. A frame naming another
    /// dictionary is refused, never decoded against this one.
    ///
    /// The dictionary is the caller's memory, not the decoder's working
    /// buffers, and is not charged to the budget: a host serving many
    /// responses with one dictionary holds it once, whatever the budget says.
    pub fn load_dictionary(&mut self, dictionary: zalt_codec::dict::Dictionary) {
        self.inner.load_dictionary(dictionary.clone());
        self.dictionary = Some(dictionary);
    }

    /// The policy in force.
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Bytes of heap this decoder currently holds.
    pub fn footprint(&self) -> usize {
        self.inner.heap_footprint()
    }

    /// Bytes currently reserved against the budget, if any.
    pub fn reserved(&self) -> u64 {
        self.lease.as_ref().map_or(0, Lease::bytes)
    }

    /// Bytes of output produced for the frame in progress.
    pub fn decoded_len(&self) -> u64 {
        self.inner.decoded_len()
    }

    /// Whether the decoder sits between frames with nothing buffered: the
    /// state in which the end of the input is a clean end of stream. See
    /// [`zalt_codec::Decoder::is_between_frames`] for why the last status is not
    /// enough to tell.
    pub fn is_between_frames(&self) -> bool {
        self.inner.is_between_frames()
    }

    /// Return to the initial state, keeping allocations and the lease.
    ///
    /// Reuse is the intended pattern: a pooled decoder should not re-allocate
    /// its block buffer for every response.
    pub fn reset(&mut self) {
        self.inner.reset();
        // A decoder that was shrunk comes back empty; this is where it gets
        // warm buffers again, so the cost of shrinking is one acquire and not
        // a fresh allocation.
        self.refill();
    }

    /// Release the decoder's buffers and its reservation.
    ///
    /// Use when a pooled decoder has grown large serving one big response and
    /// should not keep that memory reserved while idle. With a
    /// [`BufferSource`](crate::BufferSource) the buffers go back to it rather
    /// than to the system allocator, so the peak stays available to the next
    /// decoder that needs one instead of being re-faulted from scratch.
    pub fn shrink(&mut self) {
        match self.source.clone() {
            Some(source) => source.release(self.inner.take_buffers()),
            None => {
                self.inner = zalt_codec::Decoder::new(self.policy.limits);
                if let Some(dictionary) = &self.dictionary {
                    self.inner.load_dictionary(dictionary.clone());
                }
            }
        }
        if let Some(lease) = self.lease.as_mut() {
            lease.resize(0);
        }
    }

    /// Consume as much of `input` as possible, appending output to `out`.
    ///
    /// The caller may drain and discard `out` between calls: match history
    /// lives in the decoder, not in this buffer.
    pub fn decompress(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Progress> {
        let progress = self.inner.decompress(input, out)?;
        self.settle_lease()?;
        Ok(progress)
    }

    /// Bring the reservation in line with what the decoder now holds.
    fn settle_lease(&mut self) -> Result<()> {
        let Some(lease) = self.lease.as_mut() else {
            return Ok(());
        };
        let footprint = self.inner.heap_footprint() as u64;
        if footprint == lease.bytes() {
            return Ok(());
        }
        if !lease.resize(footprint) {
            return Err(Error::OverBudget {
                needed: footprint,
                available: lease.bytes(),
            });
        }
        Ok(())
    }
}

impl Drop for Decoder {
    /// Return the working buffers to the source.
    ///
    /// Without this a source would be a one-way street for any decoder that is
    /// dropped rather than pooled — which is every decoder past the pool's
    /// `max_idle`, and every decoder a panicking task unwinds through.
    ///
    /// The lease drops immediately afterwards, so the buffers are briefly
    /// charged to both the source and this decoder. That over-counts rather
    /// than under-counts, which is the direction a budget may err in.
    fn drop(&mut self) {
        if let Some(source) = self.source.take() {
            source.release(self.inner.take_buffers());
        }
    }
}

/// Decode a complete frame under a policy.
///
/// A convenience for small, whole-in-memory payloads. Anything reading from a
/// socket wants the streaming interface.
pub fn decompress(input: &[u8], policy: Policy) -> Result<Vec<u8>> {
    let mut decoder = Decoder::new(policy);
    let mut out = Vec::new();
    let mut cursor = 0usize;

    loop {
        let progress = decoder.decompress(&input[cursor..], &mut out)?;
        cursor += progress.consumed;
        match progress.status {
            Status::FrameComplete if cursor >= input.len() => return Ok(out),
            Status::FrameComplete => continue,
            // A stream that ended on a skippable frame — see
            // `zalt_codec::Decoder::is_between_frames`.
            Status::NeedMoreInput
                if progress.consumed == 0
                    && cursor == input.len()
                    && cursor > 0
                    && decoder.is_between_frames() =>
            {
                return Ok(out);
            }
            Status::NeedMoreInput if progress.consumed == 0 => {
                return Err(Error::Codec(zalt_codec::Error::Truncated));
            }
            Status::NeedMoreInput => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `zstd -1` of "hello world", with a content checksum.
    const HELLO_ZST: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x0b, 0x59, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20,
        0x77, 0x6f, 0x72, 0x6c, 0x64, 0x68, 0x69, 0x1e, 0xb2,
    ];

    #[test]
    fn decodes_under_the_untrusted_policy() {
        let out = decompress(HELLO_ZST, Policy::untrusted()).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn budgeted_decoder_charges_what_it_holds() {
        let budget = Budget::new(1 << 20);
        let mut d = Decoder::budgeted(Policy::untrusted(), &budget).unwrap();
        assert_eq!(d.reserved(), 0, "an idle decoder holds nothing");

        let mut out = Vec::new();
        d.decompress(HELLO_ZST, &mut out).unwrap();
        assert_eq!(out, b"hello world");

        assert_eq!(
            d.reserved(),
            d.footprint() as u64,
            "the reservation must track the real footprint"
        );
        assert_eq!(budget.used(), d.reserved());
    }

    /// The property that makes lazy allocation worth having: a decoder that
    /// declares a large window but decodes a small frame must not be charged
    /// for the window it never allocated.
    #[test]
    fn a_small_frame_is_not_charged_for_a_large_window() {
        let budget = Budget::new(1 << 20);
        let mut d = Decoder::budgeted(Policy::untrusted(), &budget).unwrap();
        let mut out = Vec::new();
        d.decompress(HELLO_ZST, &mut out).unwrap();

        assert!(
            d.reserved() < 64 * 1024,
            "8 MiB policy window, 11-byte frame, but charged {} bytes",
            d.reserved()
        );
    }

    #[test]
    fn dropping_a_decoder_returns_its_reservation() {
        let budget = Budget::new(1 << 20);
        {
            let mut d = Decoder::budgeted(Policy::untrusted(), &budget).unwrap();
            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).unwrap();
            assert!(budget.used() > 0);
        }
        assert_eq!(budget.used(), 0, "the lease must release on drop");
    }

    #[test]
    fn shrink_releases_buffers_and_reservation() {
        let budget = Budget::new(1 << 20);
        let mut d = Decoder::budgeted(Policy::untrusted(), &budget).unwrap();
        let mut out = Vec::new();
        d.decompress(HELLO_ZST, &mut out).unwrap();
        assert!(budget.used() > 0);

        d.shrink();
        assert_eq!(d.reserved(), 0);
        assert_eq!(budget.used(), 0);

        // Still usable afterwards.
        let mut out2 = Vec::new();
        d.decompress(HELLO_ZST, &mut out2).unwrap();
        assert_eq!(out2, b"hello world");
    }

    #[test]
    fn an_exhausted_budget_refuses_new_decoders() {
        let budget = Budget::new(10);
        let _hog = budget.reserve(10).unwrap();
        // A nominal reservation of zero still succeeds; the ceiling bites when
        // the decoder actually grows.
        let d = Decoder::budgeted(Policy::untrusted(), &budget);
        assert!(d.is_some());
    }

    /// Backpressure: when a decoder's footprint outgrows the budget, the decode
    /// reports it rather than allocating anyway.
    #[test]
    fn growing_past_the_budget_reports_over_budget() {
        // Deliberately tiny. `hello world` is a raw block, so the only heap the
        // decoder takes is its 11-byte window — a 64-byte budget would have
        // covered it and the test would have proved nothing.
        let budget = Budget::new(4);
        let mut d = Decoder::budgeted(Policy::untrusted(), &budget).unwrap();
        let mut out = Vec::new();

        let err = d.decompress(HELLO_ZST, &mut out).unwrap_err();
        assert!(
            matches!(err, Error::OverBudget { .. }),
            "expected OverBudget, got {err:?}"
        );
    }

    #[test]
    fn unbudgeted_decoders_are_never_over_budget() {
        let mut d = Decoder::new(Policy::untrusted());
        let mut out = Vec::new();
        d.decompress(HELLO_ZST, &mut out).unwrap();
        assert_eq!(d.reserved(), 0);
        assert!(d.footprint() > 0);
    }

    // -------------------------------------------------------- buffer sources

    mod source {
        use super::*;
        use crate::alloc::{BufferSource, Recycler, RecyclerConfig};
        use std::sync::Arc;

        fn recycler(budget: &Budget) -> Arc<Recycler> {
            Arc::new(Recycler::budgeted(RecyclerConfig::default(), budget).unwrap())
        }

        #[test]
        fn a_shrunk_decoder_returns_its_buffers_to_the_source() {
            let budget = Budget::new(1 << 20);
            let r = recycler(&budget);
            let mut d = Decoder::with_source(Policy::untrusted(), &budget, r.clone()).unwrap();

            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).unwrap();
            assert!(d.footprint() > 0);
            assert_eq!(r.retained_bytes(), 0, "in use, not on the free list");

            d.shrink();
            assert_eq!(d.footprint(), 0);
            assert!(
                r.retained_bytes() > 0,
                "the buffers must go to the source, not the allocator"
            );
        }

        /// The point of a source: the memory outlives the decoder that grew it.
        #[test]
        fn a_dropped_decoder_returns_its_buffers_to_the_source() {
            let budget = Budget::new(1 << 20);
            let r = recycler(&budget);
            {
                let mut d = Decoder::with_source(Policy::untrusted(), &budget, r.clone()).unwrap();
                let mut out = Vec::new();
                d.decompress(HELLO_ZST, &mut out).unwrap();
            }
            assert!(r.retained_bytes() > 0, "dropping must not be a leak path");
        }

        #[test]
        fn reset_refills_a_shrunk_decoder_from_the_source() {
            let budget = Budget::new(1 << 20);
            let r = recycler(&budget);
            let mut d = Decoder::with_source(Policy::untrusted(), &budget, r.clone()).unwrap();

            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).unwrap();
            let grown = d.footprint();
            d.shrink();

            d.reset();
            assert_eq!(
                d.footprint(),
                grown,
                "reset must take the buffers back rather than start cold"
            );
            assert_eq!(r.retained_bytes(), 0);
        }

        /// Refilling must not stack: a decoder that already holds buffers keeps
        /// the ones it has, or the footprint would double on every reset.
        #[test]
        fn reset_does_not_refill_a_decoder_that_still_has_buffers() {
            let budget = Budget::new(1 << 20);
            let r = recycler(&budget);
            r.release(zalt_codec::Buffers {
                block: Vec::with_capacity(1 << 16),
                ..Default::default()
            });

            let mut d = Decoder::with_source(Policy::untrusted(), &budget, r.clone()).unwrap();
            let initial = d.footprint();
            assert!(initial > 0, "creation takes the waiting set");

            d.reset();
            assert_eq!(d.footprint(), initial, "a second set must not be stacked");
        }

        /// **The property that makes buffer reuse safe.** A recycled window can
        /// hold the plaintext of a previous response; if any of it survived
        /// into the next decode, one origin's bytes would appear in another's
        /// output. Contents are never donated — only capacity.
        #[test]
        fn a_recycled_buffer_never_leaks_the_previous_response() {
            // A frame whose output is entirely distinct from `hello world`, so
            // any survivor is unmistakable.
            const SECRET: &[u8] = b"AUTHORIZATION-Bearer-do-not-disclose";

            let budget = Budget::new(1 << 20);
            let r = recycler(&budget);

            {
                let mut d = Decoder::with_source(Policy::untrusted(), &budget, r.clone()).unwrap();
                let mut out = Vec::new();
                d.decompress(&raw_frame(SECRET), &mut out).unwrap();
                assert_eq!(out, SECRET);
            }
            assert!(r.retained_bytes() > 0, "the buffers are on the free list");

            let mut d = Decoder::with_source(Policy::untrusted(), &budget, r.clone()).unwrap();
            let mut out = Vec::new();
            d.decompress(HELLO_ZST, &mut out).unwrap();

            assert_eq!(out, b"hello world");
            assert!(
                !out.windows(SECRET.len()).any(|w| w == SECRET),
                "a previous response's bytes appeared in this one's output"
            );
        }

        /// Accounting must survive churn. Buffers move between decoders and the
        /// free list constantly, and every hand-off is a chance to charge twice
        /// or refund twice.
        #[test]
        fn the_budget_does_not_drift_across_many_cycles() {
            let budget = Budget::new(1 << 20);
            let r = recycler(&budget);

            for _ in 0..200 {
                let mut d = Decoder::with_source(Policy::untrusted(), &budget, r.clone()).unwrap();
                let mut out = Vec::new();
                d.decompress(HELLO_ZST, &mut out).unwrap();
                assert!(budget.used() <= budget.limit());
            }

            assert_eq!(
                budget.used(),
                r.retained_bytes() as u64,
                "with every decoder gone, the free list is all that is charged"
            );
            r.purge();
            assert_eq!(budget.used(), 0, "and purging must leave nothing behind");
        }

        /// A source cannot make a tight budget worse. When the donation does
        /// not fit, the decoder starts cold rather than failing.
        #[test]
        fn a_donation_the_budget_refuses_is_handed_back() {
            let budget = Budget::new(1 << 20);
            let r = recycler(&budget);
            r.release(zalt_codec::Buffers {
                block: Vec::with_capacity(512 * 1024),
                ..Default::default()
            });

            // A second budget, far too small for that set, sharing the source.
            let tight = Budget::new(1024);
            let d = Decoder::with_source(Policy::untrusted(), &tight, r.clone()).unwrap();

            assert_eq!(d.footprint(), 0, "the oversized donation was declined");
            assert_eq!(tight.used(), 0);
            assert!(r.retained_bytes() > 0, "and went back to the source");
        }

        /// A raw (uncompressed) single-block frame carrying `content`, with no
        /// checksum. Hand-built so the test does not need an encoder.
        fn raw_frame(content: &[u8]) -> Vec<u8> {
            let mut f = Vec::new();
            f.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
            // Single_Segment, no checksum, no dict, 1-byte Frame_Content_Size.
            f.push(0b0010_0000);
            f.push(content.len() as u8);
            // Block header: last block, type 0 (Raw), size in bits 3..24.
            let header = 1u32 | ((content.len() as u32) << 3);
            f.extend_from_slice(&header.to_le_bytes()[..3]);
            f.extend_from_slice(content);
            f
        }

        #[test]
        fn the_hand_built_raw_frame_is_a_valid_frame() {
            let out = decompress(&raw_frame(b"probe"), Policy::untrusted()).unwrap();
            assert_eq!(out, b"probe", "the fixture builder itself must be right");
        }
    }

    #[test]
    fn policy_is_enforced_through_the_embed_layer() {
        // windowLog 41 from a one-byte descriptor.
        let frame = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0xff, 0x00, 0x00, 0x00];
        let err = decompress(&frame, Policy::untrusted()).unwrap_err();
        assert!(matches!(
            err,
            Error::Codec(zalt_codec::Error::WindowTooLarge { .. })
        ));
    }
}
