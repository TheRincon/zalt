# zalt

A clean-room **Zstandard (RFC 8878)** implementation in Rust, built to be
embedded in a hostile-input setting — a browser, an app, anything that handles
bytes it did not create.

Three things it tries to be, in order. Each is useful without the ones below it:

1. A clean, general, reliable Rust zstd implementation — modern format only, no
   legacy cruft.
2. An embeddable library — streaming, memory-bounded, hostile-input-safe.
3. A tuned specialization for Apple Silicon, and separately for specific content
   classes, expressed as **data** rather than code.

The design rationale lives in [`doc/ZALT-MAC.md`](doc/ZALT-MAC.md). It is the
real document; this file is the map.

---

## Status

> **pre-release (`0.0.1`).** Not published to crates.io. 

| Milestone | | |
|---|---|---|
| M0 | Skeleton, backend seam, CI | ✅ |
| M1 | Correct scalar decoder, conformance, fuzzing | ✅ |
| M2 | Embedding: budget, pool, allocators, C FFI, CLI | ✅ |
| M3 | Apple Silicon tuning | ✅ |
| M4 | Recipes and dictionaries | ✅ |
| M5 | Encoder — correctness first, then ratio | ✅ |

---

## What it does and does not do

**Does**

- Decode modern zstd frames, bit-exactly, including 4-stream Huffman literals,
  FSE-coded sequences, RLE and raw blocks, skippable frames, and the XXH64
  content checksum.
- Stream: consume socket-sized chunks and suspend/resume at *arbitrary* byte
  boundaries — mid-header, mid-block, mid-literals.
- Bound memory: a window cap checked at header parse, lazy window allocation,
  and a global budget across live decoders.
- Expose a C ABI with a hand-written header and a panic-proof boundary.
- Encode modern zstd frames, bit exactly.
- Dictionaries

**Not yet**

- **Seekable.** 

**Deliberately never**

- **Legacy frames.** zstd v0.1–v0.7 are unsupported. This is a simplification
  the reference library cannot make and we can.
- **Runtime CPU dispatch.** NEON is mandatory on aarch64; backend selection is
  compile-time.

---

## Quick start

```sh
just              # list every recipe
just t            # fast inner-loop tests (nextest, parallel, sub-second)
just gate         # everything CI runs, in CI order
just hooks-install  # activate the pre-push hook (once per clone)
```

Requires a stable Rust toolchain (MSRV **1.85**, edition 2024),
[`just`](https://github.com/casey/just) and
[`cargo-nextest`](https://nexte.st/). `zstd` 1.5.7 is used as the reference for
differential testing and benchmarks.

### As a library

```rust
use zalt_embed::{Policy, decompress};

// `Policy::untrusted()` is the restrictive default: an 8 MiB window ceiling,
// appropriate to anything arriving over a network.
let plaintext = decompress(&frame, Policy::untrusted())?;
```

For anything reading from a socket, use the streaming `Decoder` instead — it
appends to a buffer the caller may drain and discard between calls, because
match history lives in the decoder rather than in that buffer.

### As a CLI

```sh
just zalt info file.zst          # frame and block structure, without decoding
just zalt d --in-buf 1 file.zst  # decode a byte at a time, exercising every boundary
just zalt check *.zst            # decode and discard; non-zero if any input fails
```

---

## The layering

The wire format is not ours to choose, so the freedom is in the layering. **The
dependency rule: dependencies point downward only, and no lower layer may name a
higher one.** This is enforced in CI by `ci/layering.sh`, which greps the lower
crates for higher-layer vocabulary and fails the build on a hit.

| Layer | Crate | Concern | Knows about |
|---|---|---|---|
| L1 | `zalt-codec` | RFC 8878: parse and decode | Bytes. Nothing else. |
| L1a | `zalt-neon` | SIMD kernels behind the same interfaces | CPU architecture |
| L2 | `zalt-embed` | Streaming, budget, pooling, allocators | Host process constraints |
| — | `zalt-ffi` | C ABI, hand-written header | The boundary |
| — | `zalt-cli` | `zalt(1)` | Files and flags |
| — | `zalt-bench` | Throughput harness, profiling | Corpora |

`zalt-codec` is `no_std + alloc` and architecture-neutral: it may not contain
the words `neon`, `aarch64`, `darwin`, or any content class. Acceleration
reaches it only through the `backend` trait seam, and the scalar implementation
is never deleted — it is the Miri-testable oracle and the portability story.

Crates arriving later (`zalt-recipe`, `zalt-ztml`, `zalt-darwin`) are
deliberately absent rather than empty.

---

## Safety

The strongest argument for writing this in Rust is that historically, browser
decompressor CVEs are memory-safety bugs in C.

- `zalt-codec`, `zalt-embed` and `zalt-cli` are `#![forbid(unsafe_code)]`.
  Every byte the codec parses is attacker-controlled, so the parsing and
  table-construction paths carry no unsafe at all.
- **Only `zalt-neon` and `zalt-ffi` may contain `unsafe`**, and CI enforces that
  list by grep. Adding a third name to it is meant to be an argument, not an
  edit.
- `panic = "unwind"` is deliberate. This library is embedded in someone else's
  process, and aborting would let one malformed subresource take down a
  browser's whole network process. `zalt-ffi` wraps every entry point in
  `catch_unwind` and returns an error code.

---

## Performance

Decode throughput on an M1 MacBook Air, quiet machine, best of nine, against
`zstd -b3` as the reference. Both sides decode checksum-free frames in memory
through a reused context, which is the only shape in which the comparison means
anything.

| workload | zalt | `zstd -b3` | gap |
|---|---:|---:|---:|
| 61 MB of varied Rust source | **698 MB/s** | 1966 MB/s | 2.82x |
| 3.4–11.8 KB structured documents | **214 MB/s** | 504 MB/s | 2.36x |

Reproduce with `just bench-corpus` then `just measure corpus 9`. **Read
[§10's measurement protocol](doc/ZALT-MAC.md) before quoting any of this** —
these figures have been wrong more than once, always for reasons that had
nothing to do with the code. In particular: `just measure` stamps the machine
state on its output and warns when the machine is not quiet, comparing two
versions requires alternating two binaries rather than measuring across edits,
and `zstd -t` is the wrong reference because it spawns a process per iteration.

The remaining gap is largely structural: libzstd's inner loops use raw pointers
with no bounds checks, and roughly 48% of decode time here is in loops that
compare-and-branch on every slice access.

---

## Testing

`just gate` runs what CI runs, in CI order: **294 tests**, plus fmt, clippy, the
layering contract, an ABI drift check, and the whole suite again in release mode
— where `debug_assert!` is compiled out and the NEON kernels' safety contracts
are unchecked.

| | what it pins |
|---|---|
| `zstd-fixtures/golden-decompression/` | frames that must decode exactly |
| `zstd-fixtures/golden-decompression-errors/` | frames that must be **refused** |
| `just miri` | UB detection over the scalar path (Miri cannot run NEON) |
| `just no-std` | `zalt-codec` still cross-compiles to `thumbv7em-none-eabi` |
| `just ffi-smoke` | a real C program compiles and links against the staticlib |
| `fuzz/` | hostile input, differential vs libzstd, round-trip, streaming and backend equivalence |

**The errors directory is the important one.** Round-trip tests only ever prove
a decoder is permissive enough; nothing in them detects a decoder that is too
permissive. A sustained fuzz campaign found two real validation bugs that way,
both now pinned as regression tests.

---

## Repository layout

```
crates/          the workspace (see the layering table above)
doc/ZALT-MAC.md  the design document — rationale, milestones, every measurement
doc/rfc8878.txt  the spec, vendored
ci/              layering and ABI checks, git hooks
fuzz/            cargo-fuzz targets (its own workspace)
zstd-fixtures/   facebook/zstd's own regression corpus
corpus/          benchmark corpora — gitignored, local, rebuilt by `just bench-corpus`
justfile         one entry point for every task; CI runs the same recipes
```

---

## License

MIT OR Apache-2.0.
