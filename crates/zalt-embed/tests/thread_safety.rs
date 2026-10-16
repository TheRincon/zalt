//! The threading contract, asserted rather than assumed.
//!
//! # Why this file exists
//!
//! zalt starts no threads (§6: concurrency is the host's to arrange), and
//! `zalt-ffi`'s header states the boundary in those terms — *"the library starts
//! no threads and holds no global state, so a host is free to use its own
//! threading model without arranging anything around ours."* That promise is
//! only worth anything if the types can actually cross a thread, and nothing
//! checked that they could.
//!
//! They all can, and every one of them does so **incidentally** — no `unsafe
//! impl Send` anywhere, just fields that happen to be `Send`. That is exactly
//! the property worth pinning, because incidental is another word for
//! undefended: one `Rc` in a decoder's buffer plumbing, or one `Cell` added to
//! the frame state machine, removes it silently. The break would surface as a
//! compile error in the *host's* code, a layer up and a release later, with
//! nothing here to say when it was lost.
//!
//! `doc/ZALT-MAC.md` lists the threading model among the questions still open
//! for the first consumer — *"one decoder per response, or a shared pool?
//! Determines whether instances must be `Send`"* — and this is the half of that
//! question the library can answer on its own. Which shape GRAZER wants is still
//! theirs to pick; that both shapes remain available is ours to keep true.
//!
//! # What is deliberately not claimed
//!
//! `Sync` holds for the decoders too, and is close to meaningless for them: they
//! are `&mut`-driven state machines, so a shared reference cannot advance one.
//! It is asserted only to catch the change that would remove it by accident.
//! `Sync` on [`Budget`] and [`DecoderPool`] is the load-bearing one — those are
//! the types §6 expects several threads to hold at once.

use zalt_embed::{Budget, Decoder, DecoderPool, Policy, PoolConfig, Recycler, RecyclerConfig};

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

/// A decoder must be able to move to the thread that will drive it.
///
/// The "one decoder per response" shape: a response arrives on one thread, is
/// handed to a worker, and is decoded there.
#[test]
fn decoders_can_move_between_threads() {
    assert_send::<zalt_codec::Decoder<zalt_codec::Scalar>>();
    assert_send::<Decoder>();

    // Not only in the type system. A decoder built here, moved into a thread,
    // and used there is the actual shape of the claim.
    let mut decoder = Decoder::new(Policy::default());
    let frame = zalt_codec::encode::compress(
        b"the quick brown fox jumps over the lazy dog. "
            .repeat(40)
            .as_slice(),
        &zalt_codec::encode::EncodeParams {
            strategy: zalt_codec::encode::Strategy::Compress,
            ..Default::default()
        },
    )
    .expect("encodable");

    let handle = std::thread::spawn(move || {
        let mut out = Vec::new();
        decoder
            .decompress(&frame, &mut out)
            .expect("a frame we just wrote");
        out
    });

    let out = handle.join().expect("the worker did not panic");
    assert_eq!(
        out,
        b"the quick brown fox jumps over the lazy dog. ".repeat(40)
    );
}

/// The pool and the budget are shared, not moved, and must be `Sync` for it.
///
/// This is §6's "global budget across live decoders": a hostile page referencing
/// a hundred subresources is decoded concurrently, and the ceiling only means
/// anything if every one of those threads is charging the same accountant.
#[test]
fn the_budget_and_pool_can_be_shared_across_threads() {
    assert_send::<Budget>();
    assert_sync::<Budget>();
    assert_send::<DecoderPool>();
    assert_sync::<DecoderPool>();
    assert_send::<Recycler>();
    assert_sync::<Recycler>();
    assert_send::<RecyclerConfig>();
    assert_send::<PoolConfig>();
    assert_send::<Policy>();
}

/// Asserted to catch its accidental removal, not because it is useful.
///
/// A decoder is advanced through `&mut self`, so `Sync` buys a host nothing it
/// can act on. See the module comment.
#[test]
fn decoders_are_incidentally_sync() {
    assert_sync::<zalt_codec::Decoder<zalt_codec::Scalar>>();
    assert_sync::<Decoder>();
}

/// The encoder is a free function over borrowed bytes, so it is usable from any
/// thread by construction — there is no encoder *instance* to make `Send`.
///
/// Worth an assertion anyway: if a streaming encoder is ever written (§ M5 says
/// it "should be written when there is a caller to shape it"), this is the test
/// that will start failing to mention it.
#[test]
fn encoding_needs_no_shared_state() {
    let input = b"abcabcabc".repeat(500);
    let params = zalt_codec::encode::EncodeParams {
        strategy: zalt_codec::encode::Strategy::Compress,
        ..Default::default()
    };

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let input = input.clone();
            std::thread::spawn(move || {
                zalt_codec::encode::compress(&input, &params).expect("encodable")
            })
        })
        .collect();

    let frames: Vec<Vec<u8>> = threads
        .into_iter()
        .map(|t| t.join().expect("no panic"))
        .collect();

    // Same input, same parameters, same bytes — the encoder carries no state
    // that could make one thread's answer differ from another's.
    for frame in &frames[1..] {
        assert_eq!(
            frame, &frames[0],
            "encoding is not deterministic per thread"
        );
    }
}
